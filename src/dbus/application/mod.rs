mod api;

use std::{collections::HashMap, time::Duration};

use anyhow::{Result, anyhow};
use gpui::BackgroundExecutor;

/// How long an app is given to answer `Activate`.
///
/// The bus itself only gives up after 25 seconds, which is far too long to leave
/// the launcher sitting there. Nothing useful is cut short by being impatient:
/// every quick failure - an app id that is not a valid object path, a name with
/// no `.service` file, a service file whose binary is gone - comes back in well
/// under a tenth of a second. The only case this ends early is an app that was
/// spawned but never claimed its name, and falling back to `Exec` there is safe,
/// since apps that opt into D-Bus activation are single-instance by nature: a
/// second launch reaches the copy that is already coming up rather than starting
/// a rival one.
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(2);

/// Asks an app to activate itself, starting it by way of the bus if it is not
/// running yet.
///
/// This leaves it to the app to decide what launching means - a new window, or
/// raising the one it already has - which is its call to make rather than
/// something the launcher can usefully guess at.
///
/// The one failure that cannot be reported is an app that answers successfully
/// and then does nothing visible, as a background service might.
pub async fn activate(
  connection: &zbus::Connection,
  app_id: &str,
  executor: &BackgroundExecutor,
) -> Result<()> {
  let call = async {
    // The object path is the app id with its dots turned into slashes. An id
    // that is not a valid bus name - anything containing a hyphen, say - is
    // rejected while the proxy is built, before a call goes anywhere.
    let object_path = format!("/{}", app_id.replace('.', "/"));
    let proxy = api::ApplicationProxy::builder(connection)
      .destination(app_id)?
      .path(object_path.as_str())?
      .build()
      .await?;

    proxy.activate(HashMap::new()).await?;
    anyhow::Ok(())
  };

  let timeout = executor.timer(ACTIVATION_TIMEOUT);
  futures::pin_mut!(call);

  match futures::future::select(call, timeout).await {
    futures::future::Either::Left((result, _)) => result,
    futures::future::Either::Right(_) => Err(anyhow!(
      "App did not answer Activate within {ACTIVATION_TIMEOUT:?}"
    )),
  }
}
