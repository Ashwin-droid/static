use anyhow::{Context, Result, anyhow};
use dbus::ffidisp::{BusType as DbusBusType, Connection as DbusConnection};
use mpris::{PlaybackStatus, Player};
use zbus::blocking::{Connection, Proxy};

const DBUS_SERVICE: &str = "org.freedesktop.DBus";
const DBUS_OBJECT_PATH: &str = "/org/freedesktop/DBus";
const DBUS_INTERFACE: &str = "org.freedesktop.DBus";
const STATIC_MPRIS_BUS_NAME: &str = "org.mpris.MediaPlayer2.static";
const PEER_MPRIS_TIMEOUT_MS: i32 = 500;

fn list_other_mpris_services() -> Result<Vec<String>> {
    let connection = Connection::session().context("Failed to connect to the session D-Bus")?;
    let proxy = Proxy::new(&connection, DBUS_SERVICE, DBUS_OBJECT_PATH, DBUS_INTERFACE)
        .context("Failed to create D-Bus proxy")?;
    let names: Vec<String> = proxy.call("ListNames", &())?;
    Ok(names
        .into_iter()
        .filter(|name| name.starts_with("org.mpris.MediaPlayer2.") && name != STATIC_MPRIS_BUS_NAME)
        .collect())
}

fn peer_player_for_bus(bus_name: &str) -> Result<Player> {
    let connection = DbusConnection::get_private(DbusBusType::Session)
        .map_err(|err| anyhow!("Failed to connect D-Bus MPRIS client: {err}"))?;
    Player::new(connection, bus_name.to_string(), PEER_MPRIS_TIMEOUT_MS)
        .map_err(|err| anyhow!("Failed to initialize MPRIS peer {bus_name}: {err}"))
}

fn main() -> Result<()> {
    let buses = list_other_mpris_services()?;
    println!("buses={buses:?}");
    for bus in buses {
        match peer_player_for_bus(&bus) {
            Ok(player) => match player.get_playback_status() {
                Ok(PlaybackStatus::Playing) => println!("{bus}: Playing"),
                Ok(PlaybackStatus::Paused) => println!("{bus}: Paused"),
                Ok(PlaybackStatus::Stopped) => println!("{bus}: Stopped"),
                Err(err) => println!("{bus}: status error: {err}"),
            },
            Err(err) => println!("{bus}: init error: {err}"),
        }
    }
    Ok(())
}
