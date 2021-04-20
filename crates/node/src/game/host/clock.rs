use futures::stream::Stream;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};
use tokio::time::{sleep, Sleep};

use flo_w3gs::protocol::action::PlayerAction;
use futures::task::{Context, Poll};
use std::task::Waker;

#[derive(Debug)]
pub struct ActionTickStream {
  paused: bool,
  step: u16,
  step_duration: Duration,
  delay: Pin<Box<Sleep>>,
  actions: Vec<PlayerAction>,
  last_instant: Instant,
  resume_waker: Option<Waker>,
}

impl ActionTickStream {
  pub const MIN_STEP: u16 = 15;
  pub const MAX_STEP: u16 = 250;

  pub fn new(step: u16) -> Self {
    let step = std::cmp::max(Self::MIN_STEP, step);
    let step_duration = Duration::from_millis(step as u64);
    ActionTickStream {
      paused: false,
      step,
      step_duration,
      delay: Box::pin(sleep(step_duration)),
      actions: vec![],
      last_instant: Instant::now(),
      resume_waker: None,
    }
  }

  pub fn set_step(&mut self, value: u16) {
    self.step = std::cmp::min(Self::MAX_STEP, std::cmp::max(Self::MIN_STEP, value));
    self.step_duration = Duration::from_millis(value as u64);
    self
      .delay
      .as_mut()
      .reset((Instant::now() + self.step_duration).into());
  }

  pub fn step(&self) -> u16 {
    self.step
  }

  pub fn add_action(&mut self, action: PlayerAction) {
    self.actions.push(action)
  }

  pub fn replace_actions(&mut self, actions: Vec<PlayerAction>) {
    self.actions = actions;
  }

  pub fn pause(&mut self) {
    self.paused = true;
    self.delay.as_mut().reset(Instant::now().into());
  }

  pub fn is_paused(&self) -> bool {
    {
      self.paused
    }
  }

  pub fn resume(&mut self) {
    self.paused = false;
    self
      .delay
      .as_mut()
      .reset((Instant::now() + self.step_duration).into());
    self.resume_waker.take().map(|w| w.wake());
  }
}

#[derive(Debug)]
pub struct Tick {
  pub time_increment_ms: u16,
  pub actions: Vec<PlayerAction>,
}

impl Stream for ActionTickStream {
  type Item = Tick;

  fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
    if self.paused {
      if self.resume_waker.as_ref().map(|w| w.will_wake(cx.waker())) != Some(true) {
        self.resume_waker.replace(cx.waker().clone());
      }
      return Poll::Pending;
    }

    // Wait for the delay to be done
    futures::ready!(Pin::new(&mut self.delay).poll(cx));

    let now = self.delay.deadline();

    let delay = (tokio::time::Instant::now() - now).as_millis() as u16;

    let next = now + self.step_duration;
    self.delay.as_mut().reset(next);

    let tick = Tick {
      time_increment_ms: self.step + delay,
      actions: std::mem::replace(&mut self.actions, vec![]),
    };
    Poll::Ready(Some(tick))
  }
}
