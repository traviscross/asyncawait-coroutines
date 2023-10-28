#![cfg_attr(not(feature = "std"), no_std)]

use core::{
  fmt::Debug,
  future::Future,
  marker::{PhantomData, PhantomPinned},
  mem::MaybeUninit,
  panic::AssertUnwindSafe,
  pin::{pin, Pin},
  ptr::{addr_of_mut, drop_in_place, NonNull},
  task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

mod debug {
  use super::{Coro, Debug};
  use core::fmt;

  impl<Yield: Debug, Resume: Debug, Return, G> Debug
    for Coro<Yield, Resume, Return, G>
  {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      f.debug_struct("Coro")
        .field("state", &self.state)
        .field("run_state", &self.run_state)
        .field("_phantom", &self._phantom)
        .finish()
    }
  }
}

#[cfg(not(feature = "std"))]
#[allow(unused_macros)]
macro_rules! dbg {
    () => {};
    ($x:expr $(,)?) => { match $x { x => x } };
    ($($x:expr),+ $(,)?) => { ($($crate::dbg!($x)),+,) };
}

#[cfg(feature = "std")]
use std::panic::{catch_unwind, resume_unwind};

#[cfg(not(feature = "std"))]
fn catch_unwind<F: FnOnce() -> R + core::panic::UnwindSafe, R>(
  f: F,
) -> Result<R, core::convert::Infallible> {
  Ok(f())
}

#[cfg(not(feature = "std"))]
fn resume_unwind(_: core::convert::Infallible) -> ! {
  loop {}
}

#[doc(hidden)]
pub trait Captures<T: ?Sized> {}
impl<T: ?Sized, U: ?Sized> Captures<T> for U {}

#[derive(
  Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd,
)]
struct PhantomNotSync {
  _phantom: PhantomData<*const ()>,
}
// SAFETY: The type is trivial.
unsafe impl Send for PhantomNotSync {}
#[allow(non_upper_case_globals)]
const PhantomNotSync: PhantomNotSync =
  PhantomNotSync { _phantom: PhantomData };

const NOP_RAWWAKER: RawWaker = {
  fn nop(_: *const ()) {}
  const VTAB: RawWakerVTable =
    RawWakerVTable::new(|_| NOP_RAWWAKER, nop, nop, nop);
  RawWaker::new(&() as *const (), &VTAB)
};

fn poll_once<T>(f: impl Future<Output = T>) -> Poll<T> {
  let mut f = pin!(f);
  // SAFETY: Our raw waker does nothing.
  let waker = unsafe { Waker::from_raw(NOP_RAWWAKER) };
  let mut cx = Context::from_waker(&waker);
  f.as_mut().poll(&mut cx)
}

#[derive(Debug, Default)]
enum YielderState<Yield, Resume> {
  #[default]
  Temporary,
  Input(Resume),
  Output(Yield),
}

#[derive(Debug)]
pub struct Yielder<'s, Yield, Resume>(
  NonNull<YielderState<Yield, Resume>>,
  PhantomData<*mut &'s ()>,
);

impl<'s, Yield, Resume> Yielder<'s, Yield, Resume> {
  fn new(x: NonNull<YielderState<Yield, Resume>>) -> Self {
    Yielder::<'s, Yield, Resume>(x, PhantomData)
  }
  pub fn r#yield(
    &mut self,
    x: Yield,
  ) -> impl Future<Output = Resume> + Captures<(&'_ (), &'s ())> {
    let mut x = Some(x);
    core::future::poll_fn(move |_| {
      let state = self.0.as_ptr();
      // SAFETY: We have initialized and aligned the state.
      match unsafe { state.replace(YielderState::Temporary) } {
        YielderState::Temporary => {
          let x = x.take().unwrap();
          // SAFETY: We have initialized and aligned the state.
          unsafe { *state = YielderState::Output(x) };
          Poll::Pending
        }
        YielderState::Input(r) => Poll::Ready(r),
        _ => unreachable!(),
      }
    })
  }
}

pub struct CoroBuilder<Yield, Resume, Return, G>(
  MaybeUninit<Coro<Yield, Resume, Return, G>>,
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunState {
  NotStarted,
  Started,
  Done,
}

pub struct Coro<Yield, Resume, Return, G> {
  future: G, // May hold self-reference to `state`.
  state: YielderState<Yield, Resume>,
  run_state: RunState,
  _phantom: (
    PhantomData<(Yield, Resume, Return)>,
    PhantomPinned,
    PhantomNotSync,
  ),
}

impl<Yield, Resume, Return, G> CoroBuilder<Yield, Resume, Return, G> {
  pub fn init<'s, F>(
    self: Pin<&'s mut Self>,
    f: F,
  ) -> Pin<&'s mut Coro<Yield, Resume, Return, G>>
  where
    F: FnOnce(Yielder<'s, Yield, Resume>) -> G,
    G: Future<Output = Return>,
  {
    // SAFETY: We never move the pointee or allow others to do so.
    let dst = unsafe { &mut self.get_unchecked_mut().0 };
    let p = dst.as_mut_ptr();
    // SAFETY: We only write to maybe-uninitialized fields, and we
    // take care to not drop old maybe-uninitialized values.
    unsafe {
      // SAFETY: The pointer is created here and is not null.
      let state = NonNull::new_unchecked(addr_of_mut!((*p).state));
      state.as_ptr().write(YielderState::default());
      // SAFETY: The yielder must contain the `'s` lifetime so that it
      // cannot outlive our reference to `Self`.
      let yielder = Yielder::<'s, Yield, Resume>::new(state);
      let g = match catch_unwind(AssertUnwindSafe(|| f(yielder))) {
        Ok(x) => x,
        Err(e) => {
          drop_in_place(addr_of_mut!((*p).state));
          resume_unwind(e)
        }
      };
      addr_of_mut!((*p).future).write(g);
      addr_of_mut!((*p).run_state).write(RunState::NotStarted);
      addr_of_mut!((*p)._phantom).write((
        PhantomData,
        PhantomPinned,
        PhantomNotSync,
      ));
    }
    // SAFETY: We have initialiized all fields.
    let dst = unsafe { dst.assume_init_mut() };
    // SAFETY: The pointee is pinned because we received a pinned
    // reference to it.
    unsafe { Pin::new_unchecked(dst) }
  }
}

impl<Yield, Resume, Return, G> Coro<Yield, Resume, Return, G> {
  pub fn new() -> CoroBuilder<Yield, Resume, Return, G> {
    CoroBuilder(MaybeUninit::uninit())
  }
}

#[derive(Debug)]
pub enum Output<Yield, Return> {
  Next(Yield),
  Done(Return),
}

pub trait Resumable {
  type Yield;
  type Return;
  type Resume;

  fn is_started(&self) -> bool;
  fn is_done(&self) -> bool;

  fn feed(self: Pin<&mut Self>, x: Self::Resume);

  fn advance(
    self: Pin<&mut Self>,
  ) -> Output<Self::Yield, Self::Return>;

  fn start(
    self: Pin<&mut Self>,
  ) -> Output<Self::Yield, Self::Return> {
    assert!(!self.is_started());
    self.advance()
  }

  fn resume(
    mut self: Pin<&mut Self>,
    x: Self::Resume,
  ) -> Output<Self::Yield, Self::Return> {
    assert!(self.is_started());
    self.as_mut().feed(x);
    self.advance()
  }
}

impl<Yield, Resume, Return, G> Resumable
  for Coro<Yield, Resume, Return, G>
where
  G: Future<Output = Return>,
{
  type Yield = Yield;
  type Return = Return;
  type Resume = Resume;

  fn is_started(&self) -> bool {
    matches!(self.run_state, RunState::Started)
  }

  fn is_done(&self) -> bool {
    matches!(self.run_state, RunState::Done)
  }

  fn feed(self: Pin<&mut Self>, x: Self::Resume) {
    // SAFETY: We never move the pointee or allow others to do so.
    let this = unsafe { self.get_unchecked_mut() };
    assert!(this.is_started());
    let state = addr_of_mut!(this.state);
    // SAFETY: We have initialized and aligned the state.
    match unsafe { state.replace(YielderState::Temporary) } {
      YielderState::Temporary => {}
      _ => unreachable!(),
    }
    // SAFETY: We have initialized and aligned the state.
    unsafe { *state = YielderState::Input(x) };
  }

  fn advance(
    self: Pin<&mut Self>,
  ) -> Output<Self::Yield, Self::Return> {
    // SAFETY: We never move the pointee or allow others to do so.
    let this = unsafe { self.get_unchecked_mut() };
    this.run_state = RunState::Started;
    let ref mut g = this.future;
    // SAFETY: This is a pin projection; we're treating this field as
    // structual.  This is safe because 1) our type is `!Unpin`, 2)
    // `drop` does not move out of this field, 3) we uphold the `Drop`
    // guarantee, 4) we don't move out of the field or allow others to
    // do so.
    let g = unsafe { Pin::new_unchecked(g) };
    match poll_once(g) {
      Poll::Ready(x) => {
        let state = addr_of_mut!(this.state);
        // SAFETY: We have initialized and aligned the state.
        match unsafe { state.replace(YielderState::Temporary) } {
          YielderState::Temporary => {}
          _ => unreachable!(),
        }
        this.run_state = RunState::Done;
        Output::Done(x)
      }
      Poll::Pending => {
        let state = addr_of_mut!(this.state);
        // SAFETY: We have initialized and aligned the state.
        match unsafe { state.replace(YielderState::Temporary) } {
          YielderState::Output(x) => Output::Next(x),
          _ => unreachable!(),
        }
      }
    }
  }
}

pub trait Generator {
  type Item;
  fn next(self: Pin<&mut Self>) -> Option<Self::Item>;
}

impl<Yield, E, R> Generator for R
where
  R: Resumable<Yield = Yield, Resume = (), Return = Result<(), E>>,
{
  type Item = Result<Yield, E>;

  fn next(self: Pin<&mut Self>) -> Option<Self::Item> {
    let v = match () {
      _ if self.is_done() => return None,
      _ if !self.is_started() => self.start(),
      _ => self.resume(()),
    };
    match v {
      Output::Next(x) => Some(Ok(x)),
      Output::Done(Ok(())) => None,
      Output::Done(Err(e)) => Some(Err(e)),
    }
  }
}

pub trait MyIterator {
  type Item;
  fn next(&mut self) -> Option<Self::Item>;
}

impl<G: Generator> MyIterator for Pin<&mut G> {
  type Item = G::Item;

  fn next(&mut self) -> Option<Self::Item> {
    <G as Generator>::next(self.as_mut())
  }
}

impl<Yield, E, G> Iterator
  for Pin<&mut Coro<Yield, (), Result<(), E>, G>>
where
  Coro<Yield, (), Result<(), E>, G>:
    Generator<Item = Result<Yield, E>>,
{
  type Item = Result<Yield, E>;

  fn next(&mut self) -> Option<Self::Item> {
    <Coro<Yield, (), Result<(), E>, G> as Generator>::next(
      self.as_mut(),
    )
  }
}

/**
## Soundness tests

These examples must never compile as they would exhibit undefined
behavior and use-after-free.

```compile_fail,E0716
use coroutines_demo::*;
use core::pin::pin;
let Output::Done(mut boom) = ({
  let g = pin!(Coro::new());
  let g = g.init(|y: Yielder<(), ()>| async move { y });
  g.start()
}) else {
  unreachable!()
};
boom.r#yield(()); // Pointer is dangling here.
```

```compile_fail,E0716
use coroutines_demo::*;
use core::pin::pin;
let mut boom = None;
{
  let g = pin!(Coro::new());
  let g = g.init(|y: Yielder<(), ()>| {
    _ = boom.insert(y);
    async move {}
  });
  g.start();
}
boom.as_mut().unwrap().r#yield(()); // Pointer is dangling here.
```

```compile_fail,E0597
use coroutines_demo::*;
use core::pin::pin;
let g = pin!(Coro::new());
let mut g = g.init(|mut y: Yielder<(), &()>| {
  async move {
    let boom = y.r#yield(()).await;
    _ = y.r#yield(());
  }
});
_ = g.as_mut().start();
{
  let boom = ();
  _ = g.as_mut().resume(&boom);
}
_ = g.as_mut().resume(&()); // Pointer is dangling here.
```

```compile_fail,E0597
use coroutines_demo::*;
use core::pin::pin;
let boom = {
  let g = pin!(Coro::new());
  let mut g = g.init(|mut y: Yielder<&(), ()>| {
    async move {
      let boom = ();
      y.r#yield(&boom).await;
    }
  });
  g.as_mut().start()
};
_ = (boom,); // Pointer is dangling here.
```

```compile_fail,E0597
use coroutines_demo::*;
use core::pin::pin;
let boom = {
  let g = pin!(Coro::new());
  let mut g = g.init(|mut y: Yielder<&(), ()>| {
    let boom = ();
    async move {
      y.r#yield(&boom).await;
    }
  });
  g.as_mut().start()
};
_ = (boom,); // Pointer is dangling here.
```

## Correctness tests

These examples would not be undefined behavior if they were to
compile.  But it would be a bit strange if they did, and we don't
expect them to given our implementation.

```compile_fail,E0716
use coroutines_demo::*;
use core::pin::pin;
let g = Coro::new();
let Output::Done(mut boom) = ({
  let g = pin!(g);
  let g = g.init(|y: Yielder<(), ()>| async move { y });
  g.start()
}) else {
  unreachable!()
};
boom.r#yield(()); // OK?
```

```compile_fail,E0716
use coroutines_demo::*;
use core::pin::pin;
let g = Coro::new();
let mut boom = None;
{
  let g = pin!(g);
  let g = g.init(|y: Yielder<(), ()>| {
    _ = boom.insert(y);
    async move {}
  });
  g.start();
}
boom.as_mut().unwrap().r#yield(()); // OK?
```

```compile_fail,E0597
use coroutines_demo::*;
use core::pin::pin;
let g = pin!(Coro::new());
let boom = {
  let mut g = g.init(|mut y: Yielder<&(), ()>| {
    async move {
      let boom = ();
      y.r#yield(&boom).await;
    }
  });
  g.as_mut().start()
};
_ = (boom,); // OK?
```

```compile_fail,E0597
use coroutines_demo::*;
use core::pin::pin;
let g = pin!(Coro::new());
let boom = {
  let mut g = g.init(|mut y: Yielder<&(), ()>| {
    let boom = ();
    async move {
      y.r#yield(&boom).await;
    }
  });
  g.as_mut().start()
};
_ = (boom,); // OK?
```

*/
#[allow(dead_code)]
#[doc(hidden)]
fn test_compile_fail() {}

#[cfg(test)]
mod tests {
  use super::*;

  macro_rules! not_impl {
    ($tr:path, $ty:ty) => {
      trait Amb<T> {
        fn x() {}
      }
      impl<T> Amb<()> for T {}
      impl<T: $tr> Amb<((),)> for T {}
      _ = <$ty as Amb<_>>::x;
    };
  }

  #[test]
  fn test_coro_send() {
    fn is_send<T: Send>() {}
    is_send::<Coro<(), (), (), core::future::Ready<()>>>();
  }

  #[test]
  fn test_coro_not_sync() {
    not_impl!(Sync, Coro<(), (), (), core::future::Ready<()>>);
  }

  #[test]
  fn test_coro_not_unpin() {
    not_impl!(Unpin, Coro<(), (), (), core::future::Ready<()>>);
  }

  #[test]
  fn test_yielder_not_send() {
    not_impl!(Send, Yielder<(), ()>);
  }

  #[test]
  fn test_yielder_not_sync() {
    not_impl!(Sync, Yielder<(), ()>);
  }

  #[test]
  fn test_steps() {
    let g = pin!(Coro::new());
    let mut g =
      g.init(move |mut y: Yielder<'_, u8, u8>| async move {
        assert_eq!(!1, y.r#yield(0).await);
        assert_eq!(!2, y.r#yield(1).await);
        assert_eq!(!3, y.r#yield(2).await);
        assert_eq!(!4, y.r#yield(3).await);
        4
      });
    assert!(matches!(g.as_mut().start(), Output::Next(0)));
    assert!(matches!(g.as_mut().resume(!1), Output::Next(1)));
    assert!(matches!(g.as_mut().resume(!2), Output::Next(2)));
    assert!(matches!(g.as_mut().resume(!3), Output::Next(3)));
    assert!(matches!(g.resume(!4), Output::Done(4)));
  }

  #[test]
  fn test_evens_odds() {
    let g = pin!(Coro::new());
    let g = |i| {
      g.init(move |mut y: Yielder<'_, u8, u8>| async move {
        for x in (i..128).map(|x| x * 2) {
          assert_eq!(x + 1, y.r#yield(x).await);
        }
        u8::MAX
      })
    };
    let mut g = g(0);
    assert!(matches!(g.as_mut().start(), Output::Next(0)));
    for x in (0..128).map(|x| x * 2 + 1) {
      match g.as_mut().resume(x) {
        Output::Next(v) if x < 255 && v == x + 1 => (),
        Output::Done(u8::MAX) if x == 255 => (),
        _ => unreachable!(),
      }
    }
  }

  #[test]
  fn test_next_values() {
    use core::ops::RangeInclusive;
    let g = pin!(Coro::new());
    let g = |mut xs: RangeInclusive<u8>| {
      g.init(move |mut y: Yielder<'_, _, _>| async move {
        loop {
          match xs.next() {
            Some(x) if x < 254 && x % 2 == 0 => {
              let rx;
              (rx, xs) = y.r#yield((x, xs)).await;
              assert!(rx % 2 == 1);
            }
            Some(n) if n == 254 => break n,
            _ => unreachable!(),
          }
        }
      })
    };
    let xs = 0u8..=255;
    let mut g = g(xs);
    let mut gv = g.as_mut().start();
    while let Output::Next((gx, mut xs)) = gv {
      assert!(gx % 2 == 0);
      gv = g.as_mut().resume((xs.next().unwrap(), xs));
    }
    assert!(matches!(gv, Output::Done(254)));
  }

  #[test]
  fn test_no_yield() {
    let g = pin!(Coro::new());
    let g = g.init(move |_: Yielder<(), u8>| async move { 4u8 });
    assert!(matches!(g.start(), Output::Done(4u8)));
  }

  #[test]
  fn test_drop_1() {
    let g = pin!(Coro::new());
    let mut g = g.init(|y: Yielder<(), ()>| {
      drop(y);
      async move {}
    });
    g.as_mut().start();
    _ = &g.state;
  }

  #[test]
  fn test_drop_2() {
    let g = pin!(Coro::new());
    let mut g = g.init(move |mut y: Yielder<(), ()>| async move {
      y.r#yield(()).await;
    });
    g.as_mut().start();
    _ = &g.state;
  }

  #[test]
  fn test_generator_1() {
    let g = pin!(Coro::new());
    let mut g = g.init(move |mut y: Yielder<u8, ()>| async move {
      y.r#yield(1).await;
      y.r#yield(2).await;
      y.r#yield(3).await;
      Ok::<_, ()>(())
    });
    assert_eq!(Some(Ok(1)), g.as_mut().next());
    assert_eq!(Some(Ok(2)), g.as_mut().next());
    assert_eq!(Some(Ok(3)), g.as_mut().next());
    assert_eq!(None, g.as_mut().next());
  }

  #[test]
  fn test_generator_2() {
    let g = pin!(Coro::new());
    let mut g = g.init(move |mut y: Yielder<u8, ()>| async move {
      y.r#yield(1).await;
      y.r#yield(2).await;
      Err(())?;
      y.r#yield(3).await;
      Ok::<_, ()>(())
    });
    assert_eq!(Some(Ok(1)), g.as_mut().next());
    assert_eq!(Some(Ok(2)), g.as_mut().next());
    assert_eq!(Some(Err(())), g.as_mut().next());
    assert_eq!(None, g.as_mut().next());
  }

  #[test]
  fn test_iterator_1() {
    let g = pin!(Coro::new());
    let mut g = g.init(move |mut y: Yielder<u8, ()>| async move {
      y.r#yield(1).await;
      y.r#yield(2).await;
      y.r#yield(3).await;
      Ok::<_, ()>(())
    });
    assert_eq!(Some(Ok(1)), Iterator::next(&mut g));
    assert_eq!(Some(Ok(2)), Iterator::next(&mut g));
    assert_eq!(Some(Ok(3)), Iterator::next(&mut g));
    assert_eq!(None, Iterator::next(&mut g));
  }

  #[test]
  fn test_iterator_2() {
    let g = pin!(Coro::new());
    let mut g = g.init(move |mut y: Yielder<u8, ()>| async move {
      y.r#yield(1).await;
      y.r#yield(2).await;
      Err(())?;
      y.r#yield(3).await;
      Ok::<_, ()>(())
    });
    assert_eq!(Some(Ok(1)), Iterator::next(&mut g));
    assert_eq!(Some(Ok(2)), Iterator::next(&mut g));
    assert_eq!(Some(Err(())), Iterator::next(&mut g));
    assert_eq!(None, Iterator::next(&mut g));
  }
}
