mod api;

use anyhow::Result;
use futures::{Stream, StreamExt, stream::FuturesUnordered, try_join};
use gpui::SharedString;
use zvariant::OwnedObjectPath;

#[derive(Clone)]
pub struct BlueZ {
  pub conn: zbus::Connection,
}

impl BlueZ {
  pub async fn new(conn: &zbus::Connection) -> Result<Self> {
    Ok(Self { conn: conn.clone() })
  }

  pub async fn get_adapter(&self) -> Result<Option<Adapter>> {
    use zbus::fdo;

    let object_manager = fdo::ObjectManagerProxy::builder(&self.conn)
      .destination("org.bluez")?
      .path("/")?
      .build()
      .await?;

    let objects = object_manager.get_managed_objects().await?;

    for (path, interfaces) in objects {
      if interfaces.contains_key("org.bluez.Adapter1") {
        return Ok(Some(Adapter::new(&self.conn, path).await?));
      }
    }

    Ok(None)
  }

  pub async fn get_devices(&self) -> Result<Vec<Device>> {
    use zbus::fdo;

    let object_manager = fdo::ObjectManagerProxy::builder(&self.conn)
      .destination("org.bluez")?
      .path("/")?
      .build()
      .await?;

    let objects = object_manager.get_managed_objects().await?;

    let device_futures = objects
      .into_iter()
      .filter_map(|(path, interfaces)| {
        if interfaces.contains_key("org.bluez.Device1") {
          Some((path, interfaces))
        } else {
          None
        }
      })
      .map(|(path, _)| {
        let conn = self.conn.clone();
        async move { Device::new(&conn, path).await.ok() }
      })
      .collect::<FuturesUnordered<_>>();

    let devices = device_futures
      .filter_map(|device| async move { device })
      .collect::<Vec<_>>()
      .await;

    Ok(devices)
  }

  pub async fn interfaces_added(
    &self,
  ) -> Result<impl Stream<Item = (OwnedObjectPath, bool)> + use<>> {
    use zbus::fdo;

    let object_manager = fdo::ObjectManagerProxy::builder(&self.conn)
      .destination("org.bluez")?
      .path("/")?
      .build()
      .await?;

    let stream = object_manager.receive_interfaces_added().await?;
    Ok(stream.filter_map(|signal| async move {
      let args = signal.args().ok()?;
      let is_device = args
        .interfaces_and_properties
        .contains_key("org.bluez.Device1");
      let path = args.object_path.clone().into();
      Some((path, is_device))
    }))
  }
}

#[derive(Clone)]
pub struct Adapter {
  pub name: SharedString,
  pub address: SharedString,

  proxy: api::Adapter1Proxy<'static>,
}

impl Adapter {
  pub async fn new(conn: &zbus::Connection, path: OwnedObjectPath) -> Result<Self> {
    let proxy = api::Adapter1Proxy::builder(conn)
      .path(path)?
      .build()
      .await?;

    let (name, address) = try_join!(proxy.alias(), proxy.address())?;

    Ok(Self {
      name: SharedString::from(name),
      address: SharedString::from(address),
      proxy,
    })
  }

  pub async fn start_discovery(&self) -> Result<()> {
    self.proxy.start_discovery().await?;
    Ok(())
  }

  pub async fn stop_discovery(&self) -> Result<()> {
    self.proxy.stop_discovery().await?;
    Ok(())
  }
}

impl std::fmt::Debug for Adapter {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Adapter")
      .field("name", &self.name)
      .field("address", &self.address)
      .finish()
  }
}

#[derive(Clone)]
pub struct Device {
  pub address: SharedString,
  pub name: SharedString,
  pub connected: bool,
  pub paired: bool,
  pub battery: Option<u8>,

  path: OwnedObjectPath,
  conn: zbus::Connection,
  device_proxy: api::Device1Proxy<'static>,
}

impl Device {
  pub async fn new(conn: &zbus::Connection, path: OwnedObjectPath) -> Result<Self> {
    let device_proxy = api::Device1Proxy::builder(conn)
      .path(path.clone())?
      .build()
      .await?;

    let (address, alias, connected, paired) = try_join!(
      device_proxy.address(),
      device_proxy.alias(),
      device_proxy.connected(),
      device_proxy.paired(),
    )?;

    let battery = async {
      let builder = api::Battery1Proxy::builder(conn).path(path.clone()).ok()?;
      let proxy = builder.build().await.ok()?;
      proxy.percentage().await.ok()
    }
    .await;

    Ok(Self {
      address: SharedString::from(address),
      name: SharedString::from(alias),
      connected,
      paired,
      battery,
      path,
      conn: conn.clone(),
      device_proxy,
    })
  }

  pub async fn connect(&self) -> Result<()> {
    self.device_proxy.connect().await?;
    Ok(())
  }

  pub async fn disconnect(&self) -> Result<()> {
    self.device_proxy.disconnect().await?;
    Ok(())
  }

  pub async fn pair(&self) -> Result<()> {
    self.device_proxy.pair().await?;
    Ok(())
  }

  pub fn object_path(&self) -> &OwnedObjectPath {
    &self.path
  }

  pub async fn listen_alias_changed(&self) -> Result<impl Stream<Item = String> + use<>> {
    let stream = self.device_proxy.receive_alias_changed().await;
    Ok(stream.filter_map(|signal| async move { signal.get().await.ok() }))
  }

  pub async fn listen_connected_changed(&self) -> Result<impl Stream<Item = bool> + use<>> {
    let stream = self.device_proxy.receive_connected_changed().await;
    Ok(stream.filter_map(|signal| async move { signal.get().await.ok() }))
  }

  pub async fn listen_battery_changed(&self) -> Result<impl Stream<Item = Option<u8>> + use<>> {
    let battery_proxy = api::Battery1Proxy::builder(&self.conn)
      .path(self.path.clone())?
      .build()
      .await?;

    let stream = battery_proxy.receive_percentage_changed().await;
    Ok(stream.filter_map(|signal| async move { signal.get().await.ok().map(Some) }))
  }
}

impl std::fmt::Debug for Device {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Device")
      .field("address", &self.address)
      .field("name", &self.name)
      .field("connected", &self.connected)
      .field("paired", &self.paired)
      .field("battery", &self.battery)
      .finish()
  }
}
