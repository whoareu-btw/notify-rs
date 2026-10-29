//! D-Bus face of the notification center:
//!
//! * `org.freedesktop.Notifications` at `/org/freedesktop/Notifications`
//!   (what apps, browsers and `notify-send` talk to), and
//! * `dev.notifyrs.Daemon` at `/dev/notifyrs/Daemon` (open/close the panel,
//!   Do Not Disturb, state for waybar and scripts).

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::dbus::conn::NAME_DO_NOT_QUEUE;
use crate::dbus::{Conn, Message, MsgType, Value};
use crate::notify::{self, Center, Out};

pub const NOTIF_NAME: &str = "org.freedesktop.Notifications";
pub const NOTIF_PATH: &str = "/org/freedesktop/Notifications";
pub const NOTIF_IFACE: &str = "org.freedesktop.Notifications";
pub const CTL_NAME: &str = "dev.notifyrs.Daemon";
pub const CTL_PATH: &str = "/dev/notifyrs/Daemon";
pub const CTL_IFACE: &str = "dev.notifyrs.Daemon";

const SERVER_NAME: &str = "notify-rs";
const SPEC_VERSION: &str = "1.2";
const CAPABILITIES: [&str; 5] = ["actions", "body", "body-markup", "icon-static", "persistence"];

const ERR_UNKNOWN_METHOD: &str = "org.freedesktop.DBus.Error.UnknownMethod";
const ERR_UNKNOWN_OBJECT: &str = "org.freedesktop.DBus.Error.UnknownObject";
const ERR_INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";

const INTROSPECT_HEAD: &str = "<!DOCTYPE node PUBLIC \"-//freedesktop//DTD D-BUS Object Introspection 1.0//EN\" \"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd\">\n";

const NOTIF_XML: &str = r#"<node>
 <interface name="org.freedesktop.Notifications">
  <method name="GetCapabilities"><arg direction="out" type="as"/></method>
  <method name="Notify">
   <arg direction="in" type="s" name="app_name"/><arg direction="in" type="u" name="replaces_id"/>
   <arg direction="in" type="s" name="app_icon"/><arg direction="in" type="s" name="summary"/>
   <arg direction="in" type="s" name="body"/><arg direction="in" type="as" name="actions"/>
   <arg direction="in" type="a{sv}" name="hints"/><arg direction="in" type="i" name="expire_timeout"/>
   <arg direction="out" type="u" name="id"/>
  </method>
  <method name="CloseNotification"><arg direction="in" type="u" name="id"/></method>
  <method name="GetServerInformation">
   <arg direction="out" type="s" name="name"/><arg direction="out" type="s" name="vendor"/>
   <arg direction="out" type="s" name="version"/><arg direction="out" type="s" name="spec_version"/>
  </method>
  <signal name="NotificationClosed"><arg type="u" name="id"/><arg type="u" name="reason"/></signal>
  <signal name="ActionInvoked"><arg type="u" name="id"/><arg type="s" name="action_key"/></signal>
 </interface>
</node>"#;

const CTL_XML: &str = r#"<node>
 <interface name="dev.notifyrs.Daemon">
  <method name="Toggle"/><method name="Open"/><method name="Close"/>
  <method name="ToggleDnd"/>
  <method name="SetDnd"><arg direction="in" type="b" name="on"/></method>
  <method name="ClearAll"/>
  <method name="Dismiss"><arg direction="in" type="u" name="id"/></method>
  <method name="Invoke"><arg direction="in" type="u" name="id"/><arg direction="in" type="s" name="key"/></method>
  <method name="GetState">
   <arg direction="out" type="b" name="open"/><arg direction="out" type="b" name="dnd"/><arg direction="out" type="u" name="count"/>
  </method>
  <property name="Open" type="b" access="read"/>
  <property name="Dnd" type="b" access="read"/>
  <property name="Count" type="u" access="read"/>
  <signal name="StateChanged"><arg type="b" name="open"/><arg type="b" name="dnd"/><arg type="u" name="count"/></signal>
 </interface>
</node>"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelCmd {
    Toggle,
    Open,
    Close,
}

/// What a handled message means for the rest of the program.
#[derive(Default, Debug)]
pub struct Outcome {
    /// The list, a toast or Do Not Disturb changed: redraw.
    pub changed: bool,
    pub panel: Option<PanelCmd>,
}

pub fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Take both bus names. The error text is meant for the user.
pub fn claim_names(conn: &Conn) -> Result<(), String> {
    match conn.request_name(CTL_NAME, NAME_DO_NOT_QUEUE) {
        Ok(1) | Ok(4) => {}
        Ok(_) => return Err("notify-rs is already running".into()),
        Err(e) => return Err(e.to_string()),
    }
    match conn.request_name(NOTIF_NAME, NAME_DO_NOT_QUEUE) {
        Ok(1) | Ok(4) => Ok(()),
        Ok(_) => Err(format!(
            "another notification daemon already owns {NOTIF_NAME}{}.\n\
             Stop it first (mako, dunst, swaync, GNOME/KDE shell, ...), then start notify-rs.",
            describe_owner(conn)
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// " (mako, pid 1234)" if we can find out who has the name.
fn describe_owner(conn: &Conn) -> String {
    let Ok(Some(owner)) = conn.name_owner(NOTIF_NAME) else { return String::new() };
    let pid = conn
        .call(Message::call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetConnectionUnixProcessID",
            vec![Value::s(owner)],
        ))
        .ok()
        .and_then(|r| r.body.first().and_then(Value::as_u32));
    match pid {
        Some(pid) => {
            let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
            let comm = comm.trim();
            if comm.is_empty() {
                format!(" (pid {pid})")
            } else {
                format!(" ({comm}, pid {pid})")
            }
        }
        None => String::new(),
    }
}

/// The center plus its bus connection: every change that apps or waybar must
/// hear about goes through here.
pub struct Service {
    pub center: Center,
    pub conn: Conn,
    last_state: Option<(bool, bool, u32)>,
}

impl Service {
    pub fn new(conn: Conn) -> Service {
        Service { center: Center::new(), conn, last_state: None }
    }

    /// Send `NotificationClosed` / `ActionInvoked` for what the center reports.
    pub fn announce(&self, outs: &[Out]) {
        for o in outs {
            let _ = match o {
                Out::Closed(id, reason) => {
                    self.conn.emit(NOTIF_PATH, NOTIF_IFACE, "NotificationClosed", vec![Value::U32(*id), Value::U32(*reason)])
                }
                Out::Action(id, key) => {
                    self.conn.emit(NOTIF_PATH, NOTIF_IFACE, "ActionInvoked", vec![Value::U32(*id), Value::s(key.clone())])
                }
            };
        }
    }

    /// `StateChanged`, but only when something it reports actually changed.
    pub fn emit_state(&mut self) {
        let s = (self.center.panel_open(), self.center.dnd(), self.center.count() as u32);
        if self.last_state == Some(s) {
            return;
        }
        self.last_state = Some(s);
        let _ = self.conn.emit(CTL_PATH, CTL_IFACE, "StateChanged", vec![Value::Bool(s.0), Value::Bool(s.1), Value::U32(s.2)]);
    }

    // ---- actions coming from the UI

    pub fn dismiss(&mut self, id: u32) {
        let outs = self.center.dismiss(id);
        self.announce(&outs);
        self.emit_state();
    }

    pub fn activate(&mut self, id: u32, key: &str) {
        let outs = self.center.activate(id, key);
        self.announce(&outs);
        self.emit_state();
    }

    pub fn clear_all(&mut self) {
        let outs = self.center.clear_all();
        self.announce(&outs);
        self.emit_state();
    }

    pub fn set_dnd(&mut self, on: bool) {
        self.center.set_dnd(on);
        self.emit_state();
    }

    pub fn set_panel_open(&mut self, open: bool) {
        self.center.set_panel_open(open);
        self.emit_state();
    }

    /// Expire toasts. Returns whether anything changed and when to call again.
    pub fn tick(&mut self, now: Instant) -> (bool, Option<Instant>) {
        let (outs, next) = self.center.tick(now);
        self.announce(&outs);
        self.emit_state();
        (!outs.is_empty(), next)
    }

    // ---- messages from the bus

    /// Handle one message from the inbox. Anything not for us is ignored.
    pub fn handle(&mut self, msg: &Message, now: Instant) -> Outcome {
        let mut out = Outcome::default();
        if msg.mtype != MsgType::MethodCall {
            return out;
        }
        if self.conn.handle_peer(msg) {
            return out;
        }
        let path = msg.path.as_deref().unwrap_or("");
        let iface = msg.interface.as_deref().unwrap_or("");
        let member = msg.member.as_deref().unwrap_or("");

        if iface == "org.freedesktop.DBus.Introspectable" && member == "Introspect" {
            let xml = match path {
                NOTIF_PATH => Some(NOTIF_XML),
                CTL_PATH => Some(CTL_XML),
                "/" => Some("<node><node name=\"org\"/><node name=\"dev\"/></node>"),
                _ => None,
            };
            match xml {
                Some(x) => {
                    let _ = self.conn.reply(msg, vec![Value::s(format!("{INTROSPECT_HEAD}{x}"))]);
                }
                None => {
                    let _ = self.conn.reply_error(msg, ERR_UNKNOWN_OBJECT, path);
                }
            }
            return out;
        }

        match path {
            NOTIF_PATH => self.handle_notifications(msg, member, now, &mut out),
            CTL_PATH => self.handle_control(msg, iface, member, &mut out),
            _ => {
                let _ = self.conn.reply_error(msg, ERR_UNKNOWN_OBJECT, path);
            }
        }
        out
    }

    fn handle_notifications(&mut self, msg: &Message, member: &str, now: Instant, out: &mut Outcome) {
        match member {
            "GetCapabilities" => {
                let _ = self.conn.reply(msg, vec![Value::strings(CAPABILITIES)]);
            }
            "GetServerInformation" => {
                let _ = self.conn.reply(
                    msg,
                    vec![
                        Value::s(SERVER_NAME),
                        Value::s(SERVER_NAME),
                        Value::s(env!("CARGO_PKG_VERSION")),
                        Value::s(SPEC_VERSION),
                    ],
                );
            }
            "Notify" => match notify::parse_notify(&msg.body) {
                Ok(req) => {
                    let (id, outs) = self.center.notify(req, now, now_unix());
                    let _ = self.conn.reply(msg, vec![Value::U32(id)]);
                    self.announce(&outs);
                    self.emit_state();
                    out.changed = true;
                }
                Err(e) => {
                    let _ = self.conn.reply_error(msg, ERR_INVALID_ARGS, &e);
                }
            },
            "CloseNotification" => match msg.body.first().and_then(Value::as_u32) {
                Some(id) => {
                    let outs = self.center.close(id);
                    let _ = self.conn.reply(msg, vec![]);
                    out.changed = !outs.is_empty();
                    self.announce(&outs);
                    self.emit_state();
                }
                None => {
                    let _ = self.conn.reply_error(msg, ERR_INVALID_ARGS, "expected one uint32");
                }
            },
            _ => {
                let _ = self.conn.reply_error(msg, ERR_UNKNOWN_METHOD, member);
            }
        }
    }

    fn state_props(&self) -> Vec<(String, Value)> {
        vec![
            ("Open".to_string(), Value::Bool(self.center.panel_open())),
            ("Dnd".to_string(), Value::Bool(self.center.dnd())),
            ("Count".to_string(), Value::U32(self.center.count() as u32)),
        ]
    }

    fn handle_control(&mut self, msg: &Message, iface: &str, member: &str, out: &mut Outcome) {
        if iface == "org.freedesktop.DBus.Properties" {
            let which = msg.body.first().and_then(Value::as_str).unwrap_or("");
            match member {
                "GetAll" if which == CTL_IFACE => {
                    let _ = self.conn.reply(msg, vec![Value::dict_sv(self.state_props())]);
                }
                "Get" if which == CTL_IFACE => {
                    let prop = msg.body.get(1).and_then(Value::as_str).unwrap_or("");
                    match self.state_props().into_iter().find(|(k, _)| k == prop) {
                        Some((_, v)) => {
                            let _ = self.conn.reply(msg, vec![Value::v(v)]);
                        }
                        None => {
                            let _ = self.conn.reply_error(msg, "org.freedesktop.DBus.Error.UnknownProperty", prop);
                        }
                    }
                }
                "Set" => {
                    let _ = self.conn.reply_error(msg, "org.freedesktop.DBus.Error.PropertyReadOnly", "read-only");
                }
                _ => {
                    let _ = self.conn.reply_error(msg, ERR_UNKNOWN_METHOD, member);
                }
            }
            return;
        }
        if !iface.is_empty() && iface != CTL_IFACE {
            let _ = self.conn.reply_error(msg, ERR_UNKNOWN_METHOD, member);
            return;
        }
        match member {
            "Toggle" | "Open" | "Close" => {
                out.panel = Some(match member {
                    "Toggle" => PanelCmd::Toggle,
                    "Open" => PanelCmd::Open,
                    _ => PanelCmd::Close,
                });
                let _ = self.conn.reply(msg, vec![]);
            }
            "ToggleDnd" => {
                let now = !self.center.dnd();
                self.set_dnd(now);
                out.changed = true;
                let _ = self.conn.reply(msg, vec![]);
            }
            "SetDnd" => match msg.body.first().and_then(Value::as_bool) {
                Some(on) => {
                    self.set_dnd(on);
                    out.changed = true;
                    let _ = self.conn.reply(msg, vec![]);
                }
                None => {
                    let _ = self.conn.reply_error(msg, ERR_INVALID_ARGS, "expected a boolean");
                }
            },
            "ClearAll" => {
                self.clear_all();
                out.changed = true;
                let _ = self.conn.reply(msg, vec![]);
            }
            "Dismiss" => match msg.body.first().and_then(Value::as_u32) {
                Some(id) => {
                    self.dismiss(id);
                    out.changed = true;
                    let _ = self.conn.reply(msg, vec![]);
                }
                None => {
                    let _ = self.conn.reply_error(msg, ERR_INVALID_ARGS, "expected one uint32");
                }
            },
            "Invoke" => {
                let id = msg.body.first().and_then(Value::as_u32);
                let key = msg.body.get(1).and_then(Value::as_str);
                match (id, key) {
                    (Some(id), Some(key)) => {
                        self.activate(id, key);
                        out.changed = true;
                        let _ = self.conn.reply(msg, vec![]);
                    }
                    _ => {
                        let _ = self.conn.reply_error(msg, ERR_INVALID_ARGS, "expected (uint32, string)");
                    }
                }
            }
            "GetState" => {
                let _ = self.conn.reply(
                    msg,
                    vec![
                        Value::Bool(self.center.panel_open()),
                        Value::Bool(self.center.dnd()),
                        Value::U32(self.center.count() as u32),
                    ],
                );
            }
            _ => {
                let _ = self.conn.reply_error(msg, ERR_UNKNOWN_METHOD, member);
            }
        }
    }
}

/// Run the daemon without any window: notifications are kept and announced but
/// nothing is drawn. Used for tests and for debugging the bus side.
pub fn run_headless(mut svc: Service, rx: Receiver<Message>) {
    loop {
        let now = Instant::now();
        let (_, next) = svc.tick(now);
        let wait = next.map(|t| t.saturating_duration_since(now)).unwrap_or(Duration::from_secs(3600));
        match rx.recv_timeout(wait) {
            Ok(msg) => {
                let out = svc.handle(&msg, Instant::now());
                if let Some(cmd) = out.panel {
                    let open = match cmd {
                        PanelCmd::Toggle => !svc.center.panel_open(),
                        PanelCmd::Open => true,
                        PanelCmd::Close => false,
                    };
                    svc.set_panel_open(open);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}
