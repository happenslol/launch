// from cosmic-settings
use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use dbus_networkmanager::{
  device::{Device, SpecificDevice, wireless::WirelessDevice},
  interface::{
    device::{DeviceProxy, wireless::WirelessDeviceProxy},
    enums::{ApFlags, ApSecurityFlags, DeviceState, DeviceType},
  },
  nm::NetworkManager,
};
use futures::{
  StreamExt as _,
  stream::{FuturesOrdered, FuturesUnordered},
};
use itertools::Itertools;

use crate::network::types::{
  AccessPoint, DeviceConnection, DeviceInfo, HwAddress, KnownDeviceConnection, NetworkType,
};

pub async fn list_devices<'a>(nm: &NetworkManager<'a>) -> Result<Vec<DeviceInfo>> {
  let (devices, nm_settings) = futures::try_join!(nm.devices(), nm.settings())?;
  let conn_settings: &Vec<_> = &FuturesOrdered::from_iter(
    nm_settings
      .list_connections()
      .await?
      .into_iter()
      .map(|conn| async move { conn.get_settings().await }),
  )
  .filter_map(|res| async move { res.ok() })
  .collect()
  .await;

  let device_iter = devices.into_iter().map(|device| async move {
    let device_type = device.device_type().await.ok()?;
    if !matches!(device_type, DeviceType::Ethernet | DeviceType::Wifi) {
      return None;
    }

    let (interface, hw_address, state, available_connections) = futures::try_join!(
      device.interface(),
      device.hw_address(),
      device.state(),
      device.available_connections()
    )
    .ok()?;

    if hw_address.is_empty() {
      return None;
    }

    let (active_connection, available_connections) = futures::join!(
      async {
        let conn = device.active_connection().await?;
        let (id, uuid, state) = futures::try_join!(conn.id(), conn.uuid(), conn.state())?;

        Ok::<_, zbus::Error>((
          DeviceConnection {
            id,
            uuid: Arc::from(uuid),
            path: conn.inner().path().to_owned(),
          },
          state,
        ))
      },
      FuturesOrdered::from_iter(available_connections.into_iter().map(|conn| async move {
        let path = conn.inner().path().to_owned();
        let settings = conn.get_settings().await.ok()?;

        let id = settings
          .get("connection")?
          .get("id")?
          .downcast_ref::<String>()
          .ok()?;

        let uuid = settings["connection"]
          .get("uuid")?
          .downcast_ref::<String>()
          .ok()?;

        Some(DeviceConnection {
          id,
          uuid: Arc::from(uuid),
          path,
        })
      }),)
      .filter_map(|res| async move { res })
      .collect::<Vec<_>>()
    );

    let known_connections = conn_settings
      .iter()
      .flat_map(|conn_settings| {
        let connection = conn_settings.get("connection")?;

        let interface_name = connection
          .get("interface-name")?
          .downcast_ref::<String>()
          .ok()?;

        if interface_name != interface {
          return None;
        }

        let id = connection.get("id")?.downcast_ref::<String>().ok()?;
        let uuid = connection.get("uuid")?.downcast_ref::<String>().ok()?;

        Some(KnownDeviceConnection {
          uuid: Arc::from(uuid),
          id,
        })
      })
      .collect();

    Some(DeviceInfo {
      path: device.inner().path().to_owned(),
      device_type,
      interface,
      state,
      active_connection: active_connection.ok(),
      known_connections,
      available_connections,
    })
  });

  let devices_info = FuturesOrdered::from_iter(device_iter)
    .filter_map(|res| async move { res })
    .collect::<Vec<DeviceInfo>>()
    .await;

  Ok(devices_info)
}

pub async fn read_access_points(device: &WirelessDevice<'_>) -> Result<Vec<AccessPoint>> {
  let access_points = device.get_access_points().await?;
  let (state, hw_address) = {
    let device = device.upcast().await?;
    let state = device.state().await.unwrap_or(DeviceState::Unknown);
    let hw_address = device.hw_address().await.ok();

    (state, hw_address)
  };

  // Sort by strength and remove duplicates
  let mut aps = HashMap::<String, AccessPoint>::new();
  for ap in access_points {
    let (ssid_res, strength_res) = futures::join!(ap.ssid(), ap.strength());
    let Some((ssid, strength)) = ssid_res.ok().zip(strength_res.ok()) else {
      continue;
    };

    let ssid = String::from_utf8_lossy(&ssid.clone()).into_owned();
    if let Some(access_point) = aps.get(&ssid)
      && access_point.strength > strength
    {
      continue;
    }

    let Ok(flags) = ap.rsn_flags().await else {
      continue;
    };
    let network_type = if flags.intersects(ApSecurityFlags::KEY_MGMT_802_1X) {
      NetworkType::EAP
    } else if flags.intersects(ApSecurityFlags::KEY_MGMTPSK) {
      NetworkType::PSK
    } else if flags.is_empty() {
      NetworkType::Open
    } else {
      continue;
    };

    aps.insert(
      ssid.clone(),
      AccessPoint {
        ssid: Arc::from(ssid),
        strength,
        state,
        working: false,
        path: ap.inner().path().to_owned(),
        secured: !ap.wpa_flags().await?.is_empty(),
        wps_push: ap.flags().await?.contains(ApFlags::WPS_PBC),
        network_type,
        hw_address: hw_address
          .as_ref()
          .and_then(|str_addr| HwAddress::from_str(str_addr))
          .unwrap_or_default(),
      },
    );
  }

  let aps = aps
    .into_values()
    .sorted_by(|a, b| b.strength.cmp(&a.strength))
    .collect();

  Ok(aps)
}

pub async fn get_wifi_devices<'a>(
  nm: &NetworkManager<'a>,
) -> Result<Vec<(String, WirelessDeviceProxy<'a>)>> {
  let devices = nm.devices().await?;
  let wifi_devices = FuturesUnordered::from_iter(devices.into_iter().map(|device| async move {
    let device_type = device.device_type().await.ok()?;
    if !matches!(device_type, DeviceType::Wifi) {
      return None;
    }

    let conn = device.inner().connection().clone();
    let iface = device.interface().await.ok()?;
    let wifi_device = WirelessDeviceProxy::builder(&conn)
      .path(device.inner().path())
      .ok()?
      .build()
      .await
      .ok()?;

    Some((iface, wifi_device))
  }))
  .filter_map(|res| async move { res })
  .collect::<Vec<_>>()
  .await;

  Ok(wifi_devices)
}
