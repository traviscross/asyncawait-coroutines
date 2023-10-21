#![cfg_attr(not(feature = "std"), no_std)]

use core::{
  cell::Cell,
  fmt::Debug,
  future::Future,
  marker::{PhantomData, PhantomPinned},
  mem,
  pin::{pin, Pin},
  task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

mod debug {
  use super::{Coro, CoroK, Debug, YielderState, YielderStateCell};
  use core::fmt;

  impl<'s, T, R, U, F, G> Debug for CoroK<'s, T, R, U, F, G> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      match self {
        Self::Init(_, _) => write!(f, "Init(_, _)"),
        Self::Gen(_, _) => write!(f, "Gen(_)"),
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

enum CoroK<'s, T, R, U, F, G> {
  Init(F, PhantomData<(*mut &'s (), T, R, U)>),
  Gen(G, PhantomData<(*mut &'s (), T, R, U)>),
  Temporary,
}

pub struct Coro<'s, T, R, U, F, G> {
  g: CoroK<'s, T, R, U, F, G>, // Needs reference to `y`.
  y: YielderStateCell<T, R>,
  _p: (PhantomData<(*mut &'s (), T, R, U)>, PhantomPinned),
}

impl<'s, T, R, U, F, G> Coro<'s, T, R, U, F, G>
where
  T: 's,
  R: 's,
  F: FnOnce(Yielder<'s, T, R>) -> G,
  G: Future<Output = U>,
{
  pub fn new(f: F) -> Coro<'s, T, R, U, F, G> {
    Coro {
      g: CoroK::Init(f, PhantomData),
      y: YielderStateCell::default(),
      _p: (PhantomData, PhantomPinned),
    }
  }
}

#[derive(Debug)]
pub enum Output<'s, T, U> {
  Next(T, PhantomData<*mut &'s ()>),
  Done(U, PhantomData<*mut &'s ()>),
}

pub trait Resumable<'s> {
  type StreamOutput;
  type FinalOutput;
  type Input;

  fn initialize<'m>(self: Pin<&'m mut Self>);

  fn feed<'m>(self: Pin<&'m mut Self>, x: Self::Input);

  fn advance<'m>(
    self: Pin<&'m mut Self>,
  ) -> Output<'m, Self::StreamOutput, Self::FinalOutput>;

  fn start<'m>(
    mut self: Pin<&'m mut Self>,
  ) -> Output<'m, Self::StreamOutput, Self::FinalOutput> {
    self.as_mut().initialize();
    self.advance()
  }

  fn resume<'m>(
    mut self: Pin<&'m mut Self>,
    x: Self::Input,
  ) -> Output<'m, Self::StreamOutput, Self::FinalOutput> {
    self.as_mut().feed(x);
    self.advance()
  }
}

impl<'s, T, R, U, F, G> Resumable<'s> for Coro<'s, T, R, U, F, G>
where
  F: FnOnce(Yielder<'s, T, R>) -> G,
  G: Future<Output = U>,
  T: Debug + 's,
  R: Debug + 's,
  U: Debug,
  Self: 's,
{
  type StreamOutput = T;
  type FinalOutput = U;
  type Input = R;

  fn initialize<'m>(self: Pin<&'m mut Self>) {
    let self_ = unsafe { self.get_unchecked_mut() };
    dbg!(&self_);
    let y: &'m YielderStateCell<T, R> = dbg!(&self_.y);
    match y.replace(YielderState::Temporary) {
      YielderState::Temporary => {}
      _ => unreachable!(),
    }
    match mem::replace(&mut self_.g, CoroK::Temporary) {
      CoroK::Init(f, _) => {
        let y: &'s YielderStateCell<T, R> =
          unsafe { mem::transmute(y) };
        let yielder = Yielder::<'s, T, R>::new(y);
        self_.g = CoroK::Gen(f(yielder), PhantomData);
      }
      _ => unreachable!(),
    };
    dbg!(&self_);
  }

  fn feed<'m>(self: Pin<&'m mut Self>, x: Self::Input) {
    let self_ = unsafe { self.get_unchecked_mut() };
    dbg!(&self_);
    let y = dbg!(&self_.y);
    match y.replace(YielderState::Temporary) {
      YielderState::Temporary => {}
      _ => unreachable!(),
    }
    y.set(YielderState::Input(x));
  }

  fn advance<'m>(
    self: Pin<&'m mut Self>,
  ) -> Output<'m, Self::StreamOutput, Self::FinalOutput> {
    let self_ = unsafe { self.get_unchecked_mut() };
    dbg!(&self_);
    let CoroK::Gen(ref mut g, _) = self_.g else { unreachable!() };
    let g = unsafe { Pin::new_unchecked(g) };
    match poll_once(g) {
      Poll::Ready(u) => {
        dbg!(&self_);
        let y = dbg!(&self_.y);
        match y.replace(YielderState::Temporary) {
          YielderState::Temporary => {}
          _ => unreachable!(),
        }
        dbg!(Output::Done(u, PhantomData))
      }
      Poll::Pending => {
        dbg!(&self_);
        let y = dbg!(&self_.y);
        match y.replace(YielderState::Temporary) {
          YielderState::Output(t) => {
            dbg!(Output::Next(t, PhantomData))
          }
          _ => unreachable!(),
        }
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_steps() {
    let mut g = Coro::new(move |mut y| async move {
      assert_eq!(11, y.r#yield(0u8).await);
      assert_eq!(12, y.r#yield(1u8).await);
      assert_eq!(13, y.r#yield(2u8).await);
      assert_eq!(14, y.r#yield(3u8).await);
      dbg!(4u8)
    });
    let mut g = pin!(g);
    assert!(matches!(g.as_mut().start(), Output::Next(0u8, _)));
    assert!(matches!(g.as_mut().resume(11u8), Output::Next(1u8, _)));
    assert!(matches!(g.as_mut().resume(12u8), Output::Next(2u8, _)));
    assert!(matches!(g.as_mut().resume(13u8), Output::Next(3u8, _)));
    assert!(matches!(g.resume(14u8), Output::Done(4u8, _)));
  }

  #[test]
  fn test_no_yield() {
    let mut g =
      Coro::new(move |_: Yielder<(), u8>| async move { dbg!(4u8) });
    let g = pin!(g);
    assert!(matches!(dbg!(g.start()), Output::Done(4u8, _)));
  }

  #[cfg(False)]
  #[test]
  fn test_dangling_1() {
    let Output::Done(boom, _) = ({
      let mut g = Coro::new(|y: Yielder<(), ()>| async move { y });
      let g = pin!(g);
      g.start()
    }) else {
      unreachable!()
    };
    dbg!(&boom.0); // Pointer is dangling here.
  }

  #[cfg(False)]
  #[test]
  fn test_dangling_2() {
    let mut boom = None;
    {
      let mut g = Coro::new(|y: Yielder<(), ()>| {
        _ = boom.insert(y);
        async move {}
      });
      let g = pin!(g);
      g.start();
    }
    dbg!(&boom.unwrap().0); // Pointer is dangling here.
  }

  #[test]
  fn test_dangling_3() {
    let mut g = Coro::new(|y: Yielder<(), ()>| {
      drop(y);
      async move {}
    });
    let mut g = pin!(g);
    g.as_mut().start();
    dbg!(&g.y);
  }

  #[test]
  fn test_dangling_4() {
    let mut g = Coro::new(|mut y: Yielder<(), ()>| async move {
      y.r#yield(()).await;
    });
    let mut g = pin!(g);
    g.as_mut().start();
    dbg!(&g.y);
  }
}
