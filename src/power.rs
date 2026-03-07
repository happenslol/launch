use std::sync::Arc;

use gpui::{AnyView, App, Window, prelude::*};

use crate::{
  confirmation::{ConfirmationEvent, ConfirmationPrompt},
  dbus::{GlobalDbusConnection, logind::Logind},
  icon::IconName,
  launcher::{Launcher, RootItem},
  util::ResultExt,
};

pub fn get_items() -> Vec<RootItem> {
  vec![
    RootItem::Action {
      id: "reboot",
      icon: IconName::Reload,
      name: "Reboot".into(),
      description: "Restart the system".into(),
      terms: vec!["restart".into(), "reboot".into()],
      action: Arc::new(|launcher, window, cx| {
        show_confirmation(
          launcher,
          "Reboot?",
          window,
          cx,
          |window: &mut Window, cx: &mut App| {
            if let Some(conn) = GlobalDbusConnection::system(cx) {
              cx.spawn(async move |_| {
                Logind::reboot(&conn).await.log_err();
              })
              .detach();
            }
            window.remove_window();
          },
        );
      }),
    },
    RootItem::Action {
      id: "power-off",
      icon: IconName::Power,
      name: "Power Off".into(),
      description: "Shut down the system".into(),
      terms: vec!["shutdown".into(), "poweroff".into(), "halt".into()],
      action: Arc::new(|launcher, window, cx| {
        show_confirmation(
          launcher,
          "Power off?",
          window,
          cx,
          |window: &mut Window, cx: &mut App| {
            if let Some(conn) = GlobalDbusConnection::system(cx) {
              cx.spawn(async move |_| {
                Logind::power_off(&conn).await.log_err();
              })
              .detach();
            }
            window.remove_window();
          },
        );
      }),
    },
    RootItem::Action {
      id: "suspend",
      icon: IconName::Moon,
      name: "Suspend".into(),
      description: "Suspend the system".into(),
      terms: vec!["sleep".into(), "suspend".into()],
      action: Arc::new(|_launcher, window, cx| {
        if let Some(conn) = GlobalDbusConnection::system(cx) {
          cx.spawn(async move |_, _cx| {
            Logind::suspend(&conn).await.log_err();
          })
          .detach();
        }
        window.remove_window();
      }),
    },
  ]
}

fn show_confirmation(
  launcher: &mut Launcher,
  message: &str,
  window: &mut Window,
  cx: &mut Context<Launcher>,
  on_confirm: impl Fn(&mut Window, &mut App) + 'static,
) {
  let message = message.to_string();
  let prompt = cx.new(|cx| ConfirmationPrompt::new(message, window, cx));

  let subscription = cx.subscribe_in(
    &prompt,
    window,
    move |launcher, _, event: &ConfirmationEvent, window, cx| match event {
      ConfirmationEvent::Dismiss => {
        launcher.dismiss_overlay(window, cx);
      }
      ConfirmationEvent::Confirm => {
        on_confirm(window, cx);
      }
    },
  );

  launcher.action_overlay = Some(AnyView::from(prompt));
  launcher._subscriptions.push(subscription);
  cx.notify();
}
