#![cfg_attr(not(feature = "std"), no_std)]

use core::{
  fmt::Debug,
  future::Future,
  marker::{PhantomData, PhantomPinned},
  mem,
  pin::{pin, Pin},
  ptr::{self, addr_of_mut},
  task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

#[cfg(feature = "std")]
mod debug {
  use super::{Coro, CoroK, Debug};
  use std::fmt;

  impl<T, R, U, F, G> Debug for CoroK<T, R, U, F, G> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      match self {
        Self::Init(_, _) => write!(f, "Init(_, _)"),
        Self::Gen(_) => write!(f, "Gen(_)"),
        Self::Temporary => write!(f, "Temporary"),
      }
    }
  }

  impl<T, R, U, F, G> Debug for Coro<T, R, U, F, G>
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
#[derive(Debug)]
pub struct Yielder<T, R>(*mut YielderState<T, R>);

impl<T, R> Yielder<T, R>
where
  T: Debug,
  R: Debug,
{
  fn new(x: *mut YielderState<T, R>) -> Self {
    Yielder(x)
  }
  pub fn r#yield(
    &mut self,
    x: T,
  ) -> impl Future<Output = R> + Captures<&'_ ()> {
    dbg!(unsafe { &*self.0 });
    let mut x = Some(x);
    core::future::poll_fn(move |_| {
      match unsafe { ptr::replace(self.0, YielderState::Temporary) } {
        YielderState::Temporary => {
          unsafe {
            *self.0 = YielderState::Output(x.take().unwrap());
          }
          dbg!(unsafe { &*self.0 });
          dbg!(Poll::Pending)
        }
        YielderState::Input(r) => {
          dbg!(unsafe { &*self.0 });
          dbg!(Poll::Ready(r))
        }
        _ => unreachable!(),
      }
    })
  }
}

enum CoroK<T, R, U, F, G> {
  Init(F, PhantomData<(T, R, U)>),
  Gen(G),
  Temporary,
}

pub struct Coro<T, R, U, F, G> {
  g: CoroK<T, R, U, F, G>, // Needs reference to `y`.
  y: YielderState<T, R>,
  _p: (PhantomData<(T, R, U)>, PhantomPinned),
}

impl<T, R, U, F, G> Coro<T, R, U, F, G>
where
  F: FnOnce(Yielder<T, R>) -> G,
  G: Future<Output = U>,
{
  pub fn new(f: F) -> Coro<T, R, U, F, G> {
    Coro {
      g: CoroK::Init(f, PhantomData),
      y: YielderState::default(),
      _p: (PhantomData, PhantomPinned),
    }
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

  fn initialize(self: &mut Pin<&mut Self>);

  fn feed(self: &mut Pin<&mut Self>, x: Self::Input);

  fn advance(
    self: &mut Pin<&mut Self>,
  ) -> Output<Self::StreamOutput, Self::FinalOutput>;

  fn start(
    self: &mut Pin<&mut Self>,
  ) -> Output<Self::StreamOutput, Self::FinalOutput> {
    self.initialize();
    self.advance()
  }

  fn resume(
    self: &mut Pin<&mut Self>,
    x: Self::Input,
  ) -> Output<Self::StreamOutput, Self::FinalOutput> {
    self.feed(x);
    self.advance()
  }
}

impl<T, R, U, F, G> Resumable for Coro<T, R, U, F, G>
where
  F: FnOnce(Yielder<T, R>) -> G,
  G: Future<Output = U>,
  T: Debug,
  R: Debug,
  U: Debug,
{
  type StreamOutput = T;
  type FinalOutput = U;
  type Input = R;

  fn initialize(self: &mut Pin<&mut Self>) {
    let self_ = unsafe { self.as_mut().get_unchecked_mut() };
    dbg!(&self_);
    let y = addr_of_mut!(self_.y);
    dbg!(unsafe { &*y });
    assert!(matches!(unsafe { &*y }, YielderState::Temporary));
    match mem::replace(&mut self_.g, CoroK::Temporary) {
      CoroK::Init(f, _) => {
        let yielder = Yielder::new(y);
        self_.g = CoroK::Gen(f(yielder));
      }
      _ => unreachable!(),
    };
    dbg!(&self_);
  }

  fn feed(self: &mut Pin<&mut Self>, x: Self::Input) {
    let self_ = unsafe { self.as_mut().get_unchecked_mut() };
    dbg!(&self_);
    let y = addr_of_mut!(self_.y);
    dbg!(unsafe { &*y });
    assert!(matches!(unsafe { &*y }, YielderState::Temporary));
    unsafe { *y = YielderState::Input(x) };
  }

  fn advance(
    self: &mut Pin<&mut Self>,
  ) -> Output<Self::StreamOutput, Self::FinalOutput> {
    let self_ = unsafe { self.as_mut().get_unchecked_mut() };
    dbg!(&self_);
    let y = addr_of_mut!(self_.y);
    let CoroK::Gen(ref mut g) = self_.g else { unreachable!() };
    let g = unsafe { Pin::new_unchecked(g) };
    match poll_once(g) {
      Poll::Ready(u) => {
        dbg!(&self_);
        dbg!(unsafe { &*y });
        assert!(matches!(unsafe { &*y }, YielderState::Temporary));
        dbg!(Output::Done(u))
      }
      Poll::Pending => {
        dbg!(&self_);
        dbg!(unsafe { &*y });
        let YielderState::Output(t) =
          (unsafe { ptr::replace(y, YielderState::Temporary) })
        else {
          unreachable!()
        };
        dbg!(Output::Next(t))
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
    assert!(matches!(g.start(), Output::Next(0u8)));
    assert!(matches!(g.resume(11u8), Output::Next(1u8)));
    assert!(matches!(g.resume(12u8), Output::Next(2u8)));
    assert!(matches!(g.resume(13u8), Output::Next(3u8)));
    assert!(matches!(g.resume(14u8), Output::Done(4u8)));
  }

  #[test]
  fn test_no_yield() {
    let mut g =
      Coro::new(move |_: Yielder<(), u8>| async move { dbg!(4u8) });
    let mut g = pin!(g);
    assert!(matches!(dbg!(g.start()), Output::Done(4u8)));
  }

  #[test]
  fn test_dangling_1() {
    let Output::Done(boom) = ({
      let mut g = Coro::new(|y: Yielder<(), ()>| async move { y });
      let mut g = pin!(g);
      g.start()
    }) else {
      unreachable!()
    };
    dbg!(unsafe { &*boom.0 }); // Pointer is dangling here.
  }

  #[test]
  fn test_dangling_2() {
    let mut boom = None;
    {
      let mut g = Coro::new(|y: Yielder<(), ()>| {
        _ = boom.insert(y);
        async move {}
      });
      let mut g = pin!(g);
      g.start();
    }
    dbg!(unsafe { &*boom.unwrap().0 }); // Pointer is dangling here.
  }
}
