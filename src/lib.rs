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
        YielderState::Input(r) => {
          unsafe {
            *self.0 = YielderState::Output(x.take().unwrap());
          }
          dbg!(unsafe { &*self.0 });
          dbg!(Poll::Ready(r))
        }
        YielderState::Output(x) => {
          unsafe { *self.0 = YielderState::Output(x) };
          dbg!(unsafe { &*self.0 });
          dbg!(Poll::Pending)
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
  Streaming(T),
  Done(Option<T>, U),
}

pub trait Resumable {
  type StreamOutput;
  type FinalOutput;
  type Input;
  fn resume(
    self: &mut Pin<&mut Self>,
    x: Self::Input,
  ) -> Output<Self::StreamOutput, Self::FinalOutput>;
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
  fn resume(
    self: &mut Pin<&mut Self>,
    x: Self::Input,
  ) -> Output<Self::StreamOutput, Self::FinalOutput> {
    let self_ = self.as_mut();
    let self_ = unsafe { self_.get_unchecked_mut() };
    dbg!(&self_);
    let y = addr_of_mut!(self_.y);
    dbg!(unsafe { &*y });
    assert!(matches!(unsafe { &*y }, YielderState::Temporary));
    unsafe { *y = YielderState::Input(x) };
    match mem::replace(&mut self_.g, CoroK::Temporary) {
      CoroK::Init(f, _) => {
        let yielder = Yielder::new(y);
        self_.g = CoroK::Gen(f(yielder));
      }
      CoroK::Gen(g) => self_.g = CoroK::Gen(g),
      CoroK::Temporary => unreachable!(),
    }
    dbg!(&self_);
    let CoroK::Gen(ref mut g) = self_.g else { unreachable!() };
    let g = unsafe { Pin::new_unchecked(g) };
    match poll_once(g) {
      Poll::Ready(u) => {
        dbg!(&self_);
        dbg!(unsafe { &*y });
        let t =
          match unsafe { ptr::replace(y, YielderState::Temporary) } {
            YielderState::Output(t) => Some(t),
            YielderState::Input(_) => None,
            _ => unreachable!(),
          };
        dbg!(Output::Done(t, u))
      }
      Poll::Pending => {
        dbg!(&self_);
        dbg!(unsafe { &*y });
        let YielderState::Output(t) =
          (unsafe { ptr::replace(y, YielderState::Temporary) })
        else {
          unreachable!()
        };
        dbg!(Output::Streaming(t))
      }
    }
  }
}

#[test]
fn test() {
  let mut g = Coro::new(move |mut y| async move {
    assert_eq!(11, dbg!(y.r#yield(0u8).await));
    assert_eq!(12, dbg!(y.r#yield(1u8).await));
    assert_eq!(13, dbg!(y.r#yield(2u8).await));
    assert_eq!(14, dbg!(y.r#yield(3u8).await));
    dbg!(4u8)
  });
  let mut g = pin!(g);
  assert!(matches!(dbg!(g.resume(11u8)), Output::Streaming(0u8)));
  assert!(matches!(dbg!(g.resume(12u8)), Output::Streaming(1u8)));
  assert!(matches!(dbg!(g.resume(13u8)), Output::Streaming(2u8)));
  assert!(matches!(
    dbg!(g.resume(14u8)),
    Output::Done(Some(3u8), 4u8)
  ));
}

#[test]
fn test_no_yield() {
  let mut g =
    Coro::new(move |_: Yielder<(), u8>| async move { dbg!(4u8) });
  let mut g = pin!(g);
  assert!(matches!(dbg!(g.resume(14u8)), Output::Done(None, 4u8)));
}
