#![cfg_attr(not(feature = "std"), no_std)]

use core::{
  cell::Cell,
  fmt::Debug,
  future::Future,
  marker::{PhantomData, PhantomPinned},
  mem::MaybeUninit,
  pin::{pin, Pin},
  ptr::addr_of_mut,
  task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

mod debug {
  use super::{Coro, Debug, YielderState, YielderStateCell};
  use core::fmt;

  impl<'s, Yield: Debug, Resume: Debug, Return, G> Debug
    for Coro<'s, Yield, Resume, Return, G>
  {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      f.debug_struct("Coro")
        .field("state", &self.state)
        .field("_phantom", &self._phantom)
        .finish()
    }
  }

  impl<Yield: Debug, Resume: Debug> Debug
    for YielderStateCell<Yield, Resume>
  {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      let v = self.0.replace(YielderState::Temporary);
      let r = f.debug_tuple("YielderStateCell").field(&v).finish();
      self.0.set(v);
      r
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

#[doc(hidden)]
pub trait Captures<T: ?Sized> {}
impl<T: ?Sized, U: ?Sized> Captures<T> for U {}

const fn nop_rawwaker() -> RawWaker {
  fn nop(_: *const ()) {}
  const VTAB: RawWakerVTable =
    RawWakerVTable::new(|_| nop_rawwaker(), nop, nop, nop);
  RawWaker::new(&() as *const (), &VTAB)
}

fn poll_once<T>(f: impl Future<Output = T>) -> Poll<T> {
  let mut f = pin!(f);
  // SAFETY: Our raw waker does nothing.
  let waker = unsafe { Waker::from_raw(nop_rawwaker()) };
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

struct YielderStateCell<Yield, Resume>(
  Cell<YielderState<Yield, Resume>>,
);

impl<Yield, Resume> YielderStateCell<Yield, Resume> {
  pub fn replace(
    &self,
    x: YielderState<Yield, Resume>,
  ) -> YielderState<Yield, Resume> {
    self.0.replace(x)
  }
  pub fn set(&self, x: YielderState<Yield, Resume>) {
    self.0.set(x);
  }
}

impl<Yield, Resume> Default for YielderStateCell<Yield, Resume> {
  fn default() -> Self {
    Self(Cell::new(YielderState::default()))
  }
}

#[derive(Debug)]
pub struct Yielder<'s, Yield, Resume>(
  &'s YielderStateCell<Yield, Resume>,
  PhantomData<*mut &'s ()>,
);

impl<'s, Yield, Resume> Yielder<'s, Yield, Resume> {
  fn new(x: &'s YielderStateCell<Yield, Resume>) -> Self {
    Yielder::<'s, Yield, Resume>(x, PhantomData)
  }
  pub fn r#yield(
    &mut self,
    x: Yield,
  ) -> impl Future<Output = Resume> + Captures<(&'_ (), &'s ())> {
    let mut x = Some(x);
    core::future::poll_fn(move |_| {
      match self.0.replace(YielderState::Temporary) {
        YielderState::Temporary => {
          self.0.set(YielderState::Output(x.take().unwrap()));
          Poll::Pending
        }
        YielderState::Input(r) => Poll::Ready(r),
        _ => unreachable!(),
      }
    })
  }
}

pub struct CoroBuilder<'s, Yield, Resume, Return, G>(
  MaybeUninit<Coro<'s, Yield, Resume, Return, G>>,
);

pub struct Coro<'s, Yield, Resume, Return, G> {
  future: G, // May hold self-reference to `state`.
  state: YielderStateCell<Yield, Resume>,
  _phantom: (
    PhantomData<(*mut &'s (), Yield, Resume, Return)>,
    PhantomPinned,
  ),
}

impl<'s, Yield: 's, Resume: 's, Return, G>
  CoroBuilder<'s, Yield, Resume, Return, G>
{
  pub fn init<F>(
    // SAFETY: The `'s` lifetime here is critical for shortening the
    // corresponding lifetime in the output type.  Without this, that
    // lifetime could be too long, resulting in use-after-free.
    self: Pin<&'s mut Self>,
    f: F,
  ) -> Pin<&'s mut Coro<'s, Yield, Resume, Return, G>>
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
      addr_of_mut!((*p)._phantom).write((PhantomData, PhantomPinned));
      addr_of_mut!((*p).state).write(YielderStateCell::default());
      let state: &'s YielderStateCell<Yield, Resume> = &(*p).state;
      let yielder = Yielder::<'s, Yield, Resume>::new(state);
      let g = f(yielder);
      addr_of_mut!((*p).future).write(g);
    }
    // SAFETY: We have initialiized all fields.
    let dst = unsafe { dst.assume_init_mut() };
    // SAFETY: The pointee is pinned because we received a pinned
    // reference to it.
    unsafe { Pin::new_unchecked(dst) }
  }
}

impl<'s, Yield, Resume, Return, G>
  Coro<'s, Yield, Resume, Return, G>
{
  pub fn new() -> CoroBuilder<'s, Yield, Resume, Return, G> {
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

  fn feed(self: Pin<&mut Self>, x: Self::Resume);

  fn advance(
    self: Pin<&mut Self>,
  ) -> Output<Self::Yield, Self::Return>;

  fn start(
    self: Pin<&mut Self>,
  ) -> Output<Self::Yield, Self::Return> {
    self.advance()
  }

  fn resume(
    mut self: Pin<&mut Self>,
    x: Self::Resume,
  ) -> Output<Self::Yield, Self::Return> {
    self.as_mut().feed(x);
    self.advance()
  }
}

impl<'s, Yield: 's, Resume: 's, Return, G> Resumable
  for Coro<'s, Yield, Resume, Return, G>
where
  G: Future<Output = Return>,
{
  type Yield = Yield;
  type Return = Return;
  type Resume = Resume;

  fn feed(self: Pin<&mut Self>, x: Self::Resume) {
    // SAFETY: We never move the pointee or allow others to do so.
    let self_ = unsafe { self.get_unchecked_mut() };
    let state = &self_.state;
    match state.replace(YielderState::Temporary) {
      YielderState::Temporary => {}
      _ => unreachable!(),
    }
    state.set(YielderState::Input(x));
  }

  fn advance(
    self: Pin<&mut Self>,
  ) -> Output<Self::Yield, Self::Return> {
    // SAFETY: We never move the pointee or allow others to do so.
    let self_ = unsafe { self.get_unchecked_mut() };
    let ref mut g = self_.future;
    // SAFETY: This is a pin projection; we're treating this field as
    // structual.  This is safe because 1) our type is `!Unpin`, 2)
    // `drop` does not move out of this field, 3) we uphold the `Drop`
    // guarantee, 4) we don't move out of the field or allow others to
    // do so.
    let g = unsafe { Pin::new_unchecked(g) };
    match poll_once(g) {
      Poll::Ready(u) => {
        let state = &self_.state;
        match state.replace(YielderState::Temporary) {
          YielderState::Temporary => {}
          _ => unreachable!(),
        }
        Output::Done(u)
      }
      Poll::Pending => {
        let state = &self_.state;
        match state.replace(YielderState::Temporary) {
          YielderState::Output(t) => Output::Next(t),
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
}
