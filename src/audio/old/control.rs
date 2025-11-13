// Control operations using wpctl and pactl

use super::audio::AudioError;

pub async fn pactl_set_card_profile(id: String, profile: String) -> Result<(), AudioError> {
  tracing::debug!("pactl set-card-profile {id} {profile}");
  let output = async_process::Command::new("pactl")
    .args(["set-card-profile", id.as_str(), profile.as_str()])
    .output()
    .await
    .map_err(|e| AudioError::CommandFailed(format!("pactl failed to execute: {}", e)))?;

  if !output.status.success() {
    let stderr = String::from_utf8_lossy(&output.stderr);
    return Err(AudioError::CommandFailed(format!(
      "pactl set-card-profile failed with exit code {:?}: {}",
      output.status.code(),
      stderr.trim()
    )));
  }

  Ok(())
}

pub async fn wpctl_set_default(id: u32) -> Result<(), AudioError> {
  tracing::debug!("wpctl set-default {id}");
  let id_str = id.to_string();
  let output = async_process::Command::new("wpctl")
    .args(["set-default", id_str.as_str()])
    .output()
    .await
    .map_err(|e| AudioError::CommandFailed(format!("wpctl failed to execute: {}", e)))?;

  if !output.status.success() {
    let stderr = String::from_utf8_lossy(&output.stderr);
    return Err(AudioError::CommandFailed(format!(
      "wpctl set-default failed with exit code {:?}: {}",
      output.status.code(),
      stderr.trim()
    )));
  }

  Ok(())
}

pub async fn wpctl_set_mute(id: u32, mute: bool) -> Result<(), AudioError> {
  tracing::debug!("wpctl set-mute {id} {}", if mute { "1" } else { "0" });
  let id_str = id.to_string();
  let output = async_process::Command::new("wpctl")
    .args(["set-mute", id_str.as_str(), if mute { "1" } else { "0" }])
    .output()
    .await
    .map_err(|e| AudioError::CommandFailed(format!("wpctl failed to execute: {}", e)))?;

  if !output.status.success() {
    let stderr = String::from_utf8_lossy(&output.stderr);
    return Err(AudioError::CommandFailed(format!(
      "wpctl set-mute failed with exit code {:?}: {}",
      output.status.code(),
      stderr.trim()
    )));
  }

  Ok(())
}

pub async fn wpctl_set_volume(id: u32, volume: u32) -> Result<(), AudioError> {
  tracing::debug!("wpctl set-volume {id} {volume}");
  let id_str = id.to_string();
  let volume_str = format!("{}.{:02}", volume / 100, volume % 100);
  let output = async_process::Command::new("wpctl")
    .args(["set-volume", id_str.as_str(), volume_str.as_str()])
    .output()
    .await
    .map_err(|e| AudioError::CommandFailed(format!("wpctl failed to execute: {}", e)))?;

  if !output.status.success() {
    let stderr = String::from_utf8_lossy(&output.stderr);
    return Err(AudioError::CommandFailed(format!(
      "wpctl set-volume failed with exit code {:?}: {}",
      output.status.code(),
      stderr.trim()
    )));
  }

  Ok(())
}
