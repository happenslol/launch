mod api;

use std::collections::HashMap;

use anyhow::Result;

pub async fn activate(conn: &zbus::Connection, app_id: &str) -> Result<()> {
  let object_path = format!("/{}", app_id.replace('.', "/"));
  let proxy = api::ApplicationProxy::builder(conn)
    .destination(app_id)?
    .path(object_path.as_str())?
    .build()
    .await?;
  proxy.activate(HashMap::new()).await?;
  Ok(())
}
