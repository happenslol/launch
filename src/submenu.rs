use std::time::Duration;

use gpui::{
  Animation, AnimationExt, AnyElement, App, Context, ElementId, Entity, EventEmitter, FocusHandle,
  Focusable, IntoElement, KeyBinding, Pixels, Render, Subscription, Task, Window, actions, div,
  prelude::*, px, rgb, rgba,
};

use crate::{
  picker::{Picker, PickerDelegate, PickerEvent, picker_input, picker_results},
  util::ResultExt,
};

actions!(submenu, [DismissSubMenu]);

const CONTEXT: &str = "submenu";
const ANIM_ENTER_DURATION: Duration = Duration::from_millis(150);
const ANIM_EXIT_DURATION: Duration = Duration::from_millis(100);

pub enum SubMenuEvent {
  Dismissed,
}

type HeaderFn = Box<dyn Fn(&mut Window, &mut App) -> AnyElement>;

const DEFAULT_HEIGHT: Pixels = px(300.);

pub struct SubMenu<D: PickerDelegate> {
  picker: Entity<Picker<D>>,
  header: Option<HeaderFn>,
  height: Pixels,
  closing: bool,
  dismiss_task: Option<Task<()>>,
  _picker_subscription: Subscription,
}

impl<D: PickerDelegate> SubMenu<D> {
  pub fn new(picker: Entity<Picker<D>>, window: &mut Window, cx: &mut Context<Self>) -> Self {
    cx.bind_keys([KeyBinding::new("escape", DismissSubMenu, Some(CONTEXT))]);

    window.focus(&picker.read(cx).search_input.focus_handle(cx), cx);

    let picker_subscription = cx.subscribe_in(
      &picker,
      window,
      |this, _picker, ev: &PickerEvent<D>, _window, cx| {
        if let PickerEvent::Picked(item) = ev {
          cx.emit(PickerEvent::Picked(item.clone()));
          this.dismiss(cx);
        }
      },
    );

    Self {
      picker,
      header: None,
      height: DEFAULT_HEIGHT,
      closing: false,
      dismiss_task: None,
      _picker_subscription: picker_subscription,
    }
  }

  pub fn height(mut self, height: Pixels) -> Self {
    self.height = height;
    self
  }

  pub fn header(mut self, header: impl Fn(&mut Window, &mut App) -> AnyElement + 'static) -> Self {
    self.header = Some(Box::new(header));
    self
  }

  pub fn dismiss(&mut self, cx: &mut Context<Self>) {
    if self.closing {
      return;
    }

    self.closing = true;
    cx.notify();

    self.dismiss_task = Some(cx.spawn(async move |this, cx| {
      cx.background_executor().timer(ANIM_EXIT_DURATION).await;

      this
        .update(cx, |_this, cx| {
          cx.emit(SubMenuEvent::Dismissed);
        })
        .log_err();
    }));
  }

  pub fn picker(&self) -> &Entity<Picker<D>> {
    &self.picker
  }

  fn dismiss_action(&mut self, _: &DismissSubMenu, _window: &mut Window, cx: &mut Context<Self>) {
    self.dismiss(cx);
  }
}

impl<D: PickerDelegate> EventEmitter<SubMenuEvent> for SubMenu<D> {}
impl<D: PickerDelegate> EventEmitter<PickerEvent<D>> for SubMenu<D> {}

impl<D: PickerDelegate> Focusable for SubMenu<D> {
  fn focus_handle(&self, cx: &App) -> FocusHandle {
    self.picker.read(cx).focus_handle(cx)
  }
}

impl<D: PickerDelegate> Render for SubMenu<D> {
  fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let closing = self.closing;
    let easing = |delta: f32| 1.0 - (1.0 - delta).powi(3);

    let header_element = self.header.as_ref().map(|f| f(window, cx));
    let height = self.height;

    div()
      .absolute()
      .top_0()
      .left_0()
      .size_full()
      .flex()
      .items_end()
      .justify_end()
      .pb_4()
      .pr_4()
      .key_context(CONTEXT)
      .on_action(cx.listener(Self::dismiss_action))
      .child(
        div()
          .id("submenu-backdrop")
          .occlude()
          .absolute()
          .top_0()
          .left_0()
          .size_full()
          .rounded_xl()
          .bg(rgba(0x00000088))
          .with_animation(
            ElementId::NamedInteger("submenu-backdrop-fade".into(), closing as u64),
            Animation::new(ANIM_ENTER_DURATION).with_easing(easing),
            move |this, delta| {
              let opacity = if closing { 1.0 - delta } else { delta };
              this.opacity(opacity)
            },
          ),
      )
      .child(
        div()
          .id("submenu-content")
          .occlude()
          .w(px(400.))
          .flex()
          .flex_col()
          .gap_2()
          .when_some(header_element, |this, header| this.child(header))
          .child(
            div()
              .h(height)
              .bg(rgb(0x1D1D1D))
              .border_1()
              .border_color(rgba(0xFFFFFF15))
              .rounded_lg()
              .overflow_hidden()
              .flex()
              .flex_col()
              .child(picker_results(&self.picker).flex_grow().min_h_0())
              .child(picker_input(&self.picker).border_b_0().border_t_1()),
          )
          .with_animation(
            ElementId::NamedInteger("submenu-slide".into(), closing as u64),
            Animation::new(ANIM_ENTER_DURATION).with_easing(easing),
            move |this, delta| {
              let progress = if closing { delta } else { 1.0 - delta };
              let opacity = if closing { 1.0 - delta } else { delta };
              let scale = 0.9 + 0.1 * (1.0 - progress);
              let offset = 15.0 * progress;
              this
                .w(px(400. * scale))
                .mb(px(-offset))
                .mr(px(-offset))
                .opacity(opacity)
            },
          ),
      )
  }
}
