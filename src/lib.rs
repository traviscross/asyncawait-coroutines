#![cfg_attr(not(feature = "std"), no_std)]

use core::{
  cell::Cell,
  fmt::Debug,
  future::Future,
  marker::{PhantomData, PhantomPinned},
  mem::{self, MaybeUninit},
  pin::{pin, Pin},
  ptr::addr_of_mut,
  task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

mod debug {
  use super::{Coro, CoroK, Debug, YielderState, YielderStateCell};
  use core::fmt;

  impl<F, G> Debug for CoroK<F, G> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      match self {
        Self::Init(_) => write!(f, "Init(_, _)"),
        Self::Gen(_) => write!(f, "Gen(_)"),
        Self::Temporary => write!(f, "Temporary"),
      }
    }
  }

  impl<'s, T, R, U, F, G> Debug for Coro<'s, T, R, U, F, G>
  where
    T: Debug,
    R: Debug,
  {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      f.debug_struct("Coro")
        .field("g", &self.g)
        .field("y", &self.y)
        .field("_p", &self._p)
        .finish()
    }
  }

  impl<T: Debug, R: Debug> Debug for YielderStateCell<T, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      let v = self.0.replace(YielderState::Temporary);
      let r = f.debug_tuple("YielderStateCell").field(&v).finish();
      self.0.set(v);
      r
    }
  }
}

#[cfg(not(feature = "std"))]
macro_rules! dbg {
    () => {};
    ($x:expr $(,)?) => { match $x { x => x } };
    ($($x:expr),+ $(,)?) => { ($($crate::dbg!($x)),+,) };
}

#[doc(hidden)]
pub trait Captures<T: ?Sized> {}
impl<T: ?Sized, U: ?Sized> Captures<T> for U {}

const fn nop_rawwaker() -> RawWaker {
  fn nop(_: *const ()) {}
  const VTAB: RawWakerVTable =
    RawWakerVTable::new(|_| nop_rawwaker(), nop, nop, nop);
  RawWaker::new(&() as *const (), &VTAB)
}

fn poll_once<T: Debug>(f: impl Future<Output = T>) -> Poll<T> {
  dbg!("poll_once");
  let mut f = pin!(f);
  let waker = unsafe { Waker::from_raw(nop_rawwaker()) };
  let mut cx = Context::from_waker(&waker);
  dbg!(f.as_mut().poll(&mut cx))
}

#[derive(Debug, Default)]
enum YielderState<T, R> {
  #[default]
  Temporary,
  Input(R),
  Output(T),
}

struct YielderStateCell<T, R>(Cell<YielderState<T, R>>);

impl<T, R> YielderStateCell<T, R> {
  pub fn replace(&self, x: YielderState<T, R>) -> YielderState<T, R> {
    self.0.replace(x)
  }
  pub fn set(&self, x: YielderState<T, R>) {
    self.0.set(x);
  }
}

impl<T, R> Default for YielderStateCell<T, R> {
  fn default() -> Self {
    Self(Cell::new(YielderState::default()))
  }
}

#[derive(Debug)]
pub struct Yielder<'s, T, R>(
  &'s YielderStateCell<T, R>,
  PhantomData<*mut &'s ()>,
);

impl<'s, T, R> Yielder<'s, T, R>
where
  T: Debug,
  R: Debug,
{
  fn new(x: &'s YielderStateCell<T, R>) -> Self {
    Yielder::<'s, T, R>(x, PhantomData)
  }
  pub fn r#yield(
    &mut self,
    x: T,
  ) -> impl Future<Output = R> + Captures<(&'_ (), &'s ())> {
    dbg!(&self.0);
    let mut x = Some(x);
    core::future::poll_fn(move |_| {
      match self.0.replace(YielderState::Temporary) {
        YielderState::Temporary => {
          self.0.set(YielderState::Output(x.take().unwrap()));
          dbg!(&self.0);
          dbg!(Poll::Pending)
        }
        YielderState::Input(r) => {
          dbg!(&self.0);
          dbg!(Poll::Ready(r))
        }
        _ => unreachable!(),
      }
    })
  }
}

enum CoroK<F, G> {
  Init(F),
  Gen(G),
  Temporary,
}

pub struct CoroBuilder<'s, T, R, U, F, G>(
  MaybeUninit<Coro<'s, T, R, U, F, G>>,
);

pub struct Coro<'s, T, R, U, F, G> {
  g: CoroK<F, G>, // Needs reference to `y`.
  y: YielderStateCell<T, R>,
  _p: (PhantomData<(*mut &'s (), T, R, U)>, PhantomPinned),
}

impl<'s, T, R, U, F, G> CoroBuilder<'s, T, R, U, F, G> {
  pub fn init(
    // SAFETY: The `'s` lifetime here is critical for shortening the
    // corresponding lifetime in the output type.  Without this, that
    // lifetime could be too long, resulting in use-after-free.
    self: Pin<&'s mut Self>,
    f: F,
  ) -> Pin<&'s mut Coro<'s, T, R, U, F, G>>
  where
    F: FnOnce(Yielder<'s, T, R>) -> G,
    G: Future<Output = U>,
    T: Debug + 's,
    R: Debug + 's,
    U: Debug,
  {
    // SAFETY: We never move the pointee or allow others to do so.
    let dst = unsafe { &mut self.get_unchecked_mut().0 };
    let p = dst.as_mut_ptr();
    // SAFETY: We only write to maybe-uninitialized fields, and we
    // take care to not drop old maybe-uninitialized values.
    unsafe {
      addr_of_mut!((*p).g).write(CoroK::Init(f));
      addr_of_mut!((*p).y).write(YielderStateCell::default());
      addr_of_mut!((*p)._p).write((PhantomData, PhantomPinned));
    }
    // SAFETY: We have initialiized all fields.
    let dst = unsafe { dst.assume_init_mut() };
    // SAFETY: The pointee is pinned because we received a pinned
    // reference to it.
    unsafe { Pin::new_unchecked(dst) }
  }
}

impl<'s, T, R, U, F, G> Coro<'s, T, R, U, F, G> {
  pub fn new() -> CoroBuilder<'s, T, R, U, F, G> {
    CoroBuilder(MaybeUninit::uninit())
  }
}

#[derive(Debug)]
pub enum Output<T, U> {
  Next(T),
  Done(U),
}

pub trait Resumable {
  type StreamOutput;
  type FinalOutput;
  type Input;

  fn initialize(self: Pin<&mut Self>);

  fn feed(self: Pin<&mut Self>, x: Self::Input);

  fn advance(
    self: Pin<&mut Self>,
  ) -> Output<Self::StreamOutput, Self::FinalOutput>;

  fn start(
    mut self: Pin<&mut Self>,
  ) -> Output<Self::StreamOutput, Self::FinalOutput> {
    self.as_mut().initialize();
    self.advance()
  }

  fn resume(
    mut self: Pin<&mut Self>,
    x: Self::Input,
  ) -> Output<Self::StreamOutput, Self::FinalOutput> {
    self.as_mut().feed(x);
    self.advance()
  }
}

impl<'s, T, R, U, F, G> Resumable for Coro<'s, T, R, U, F, G>
where
  F: FnOnce(Yielder<'s, T, R>) -> G,
  G: Future<Output = U>,
  T: Debug + 's,
  R: Debug + 's,
  U: Debug,
{
  type StreamOutput = T;
  type FinalOutput = U;
  type Input = R;

  fn initialize(self: Pin<&mut Self>) {
    // SAFETY: We never move the pointee or allow others to do so.
    let self_ = unsafe { self.get_unchecked_mut() };
    dbg!(&self_);
    let y: &'_ YielderStateCell<T, R> = dbg!(&self_.y);
    match y.replace(YielderState::Temporary) {
      YielderState::Temporary => {}
      _ => unreachable!(),
    }
    match mem::replace(&mut self_.g, CoroK::Temporary) {
      CoroK::Init(f) => {
        // SAFETY: We initialized `Coro<'s, ..>` such that `'s` is
        // equal to the lifetime of some pinned mutable reference to
        // `Self`, so we know that `Self: 's`.  Since `Self` is
        // pinned, we know that it won't move out from under us for at
        // least this long.
        //
        // The remaining thing we have to worry about is the
        // possibility that a mutable reference has been handed out to
        // this data and that we'd be aliasing it.  Since we don't
        // hand out any mutable references to this field, that's not
        // possible, so this is OK.
        let y: &'s YielderStateCell<T, R> =
          unsafe { mem::transmute(y) };
        let yielder = Yielder::<'s, T, R>::new(y);
        self_.g = CoroK::Gen(f(yielder));
      }
      _ => unreachable!(),
    };
    dbg!(&self_);
  }

  fn feed(self: Pin<&mut Self>, x: Self::Input) {
    // SAFETY: We never move the pointee or allow others to do so.
    let self_ = unsafe { self.get_unchecked_mut() };
    dbg!(&self_);
    let y = dbg!(&self_.y);
    match y.replace(YielderState::Temporary) {
      YielderState::Temporary => {}
      _ => unreachable!(),
    }
    y.set(YielderState::Input(x));
  }

  fn advance(
    self: Pin<&mut Self>,
  ) -> Output<Self::StreamOutput, Self::FinalOutput> {
    // SAFETY: We never move the pointee or allow others to do so.
    let self_ = unsafe { self.get_unchecked_mut() };
    dbg!(&self_);
    let CoroK::Gen(ref mut g) = self_.g else { unreachable!() };
    // SAFETY: This is a pin projection; we're treating this field as
    // structual.  This is safe because 1) our type is `!Unpin`, 2)
    // `drop` does not move out of this field, 3) we uphold the `Drop`
    // guarantee, 4) we don't move out of the field or allow others to
    // do so.
    let g = unsafe { Pin::new_unchecked(g) };
    match poll_once(g) {
      Poll::Ready(u) => {
        dbg!(&self_);
        let y = dbg!(&self_.y);
        match y.replace(YielderState::Temporary) {
          YielderState::Temporary => {}
          _ => unreachable!(),
        }
        dbg!(Output::Done(u))
      }
      Poll::Pending => {
        dbg!(&self_);
        let y = dbg!(&self_.y);
        match y.replace(YielderState::Temporary) {
          YielderState::Output(t) => {
            dbg!(Output::Next(t))
          }
          _ => unreachable!(),
        }
      }
    }
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

```compile_fail,E0499,E0716
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

```compile_fail,E0499,E0716
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
*/
#[allow(dead_code)]
#[doc(hidden)]
fn test_compile_fail() {}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_steps() {
    let g = pin!(Coro::new());
    let mut g = g.init(move |mut y: Yielder<'_, _, _>| async move {
      assert_eq!(11, y.r#yield(0u8).await);
      assert_eq!(12, y.r#yield(1u8).await);
      assert_eq!(13, y.r#yield(2u8).await);
      assert_eq!(14, y.r#yield(3u8).await);
      dbg!(4u8)
    });
    assert!(matches!(g.as_mut().start(), Output::Next(0u8)));
    assert!(matches!(g.as_mut().resume(11u8), Output::Next(1u8)));
    assert!(matches!(g.as_mut().resume(12u8), Output::Next(2u8)));
    assert!(matches!(g.as_mut().resume(13u8), Output::Next(3u8)));
    assert!(matches!(g.resume(14u8), Output::Done(4u8)));
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
  fn test_no_yield() {
    let g = pin!(Coro::new());
    let g =
      g.init(move |_: Yielder<(), u8>| async move { dbg!(4u8) });
    assert!(matches!(dbg!(g.start()), Output::Done(4u8)));
  }

  #[test]
  fn test_drop_1() {
    let g = pin!(Coro::new());
    let mut g = g.init(|y: Yielder<(), ()>| {
      drop(y);
      async move {}
    });
    g.as_mut().start();
    dbg!(&g.y);
  }

  #[test]
  fn test_drop_2() {
    let g = pin!(Coro::new());
    let mut g = g.init(move |mut y: Yielder<(), ()>| async move {
      y.r#yield(()).await;
    });
    g.as_mut().start();
    dbg!(&g.y);
  }
}
