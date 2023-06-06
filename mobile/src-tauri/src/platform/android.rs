use std::{
    fs::File,
    io::{ErrorKind, Read, Write},
    mem::ManuallyDrop,
    os::fd::{FromRawFd, OwnedFd},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, LockResult, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard,
    },
};

use jni::{
    objects::{JClass, JObject, JString},
    sys::{jboolean, jint},
    JNIEnv, JavaVM,
};
use smoltcp::{
    phy::Medium::Ip,
    wire::{IpAddress, IpProtocol, IpVersion, Ipv4Packet, Ipv6Packet, TcpPacket, UdpPacket},
};

use crate::{emit_event, types, types::VpnError};

struct AndroidContext {
    jvm: JavaVM,
    running: Arc<AtomicBool>,
    fd: i32,
}

unsafe impl Sync for AndroidContext {}

unsafe impl Send for AndroidContext {}

lazy_static::lazy_static! {
    static ref CONTEXT:RwLock<Option<AndroidContext>> = RwLock::new(None);
}

fn get_context<'a>() -> Result<
    (
        &'a AndroidContext,
        LockResult<RwLockReadGuard<'a, Option<AndroidContext>>>,
    ),
    types::VpnError,
> {
    let lock = CONTEXT.read();
    let context = lock
        .as_ref()
        .map_err(|e| types::VpnError::RLock(e.to_string()))
        .map(|context| -> Result<&'a AndroidContext, types::VpnError> {
            unsafe {
                std::mem::transmute(context.as_ref().ok_or(types::VpnError::NoPlatformContext))
            }
        })??;
    Ok((context, lock))
}

fn get_mut_context<'a>() -> Result<
    (
        &'a mut AndroidContext,
        LockResult<RwLockWriteGuard<'a, Option<AndroidContext>>>,
    ),
    types::VpnError,
> {
    let mut lock = CONTEXT.write();
    let context = lock
        .as_mut()
        .map_err(|e| types::VpnError::WLock(e.to_string()))
        .map(
            |context| -> Result<&'a mut AndroidContext, types::VpnError> {
                unsafe {
                    std::mem::transmute(context.as_mut().ok_or(types::VpnError::NoPlatformContext))
                }
            },
        )??;
    Ok((context, lock))
}

#[no_mangle]
pub extern "system" fn Java_com_bmshi_proxy_mobile_MainActivity_00024Companion_initRust<'local>(
    env: JNIEnv<'local>,
    _: JClass<'local>,
) {
    if let Err(err) = init_rust(env) {
        log::error!("init rust failed:{:?}", err);
    }
}

fn init_rust<'local>(env: JNIEnv<'local>) -> Result<(), types::VpnError> {
    let jvm = env.get_java_vm()?;
    let mut result = CONTEXT
        .write()
        .map_err(|e| types::VpnError::WLock(e.to_string()))?;
    result.replace(AndroidContext {
        jvm,
        running: Arc::new(AtomicBool::new(false)),
        fd: -1,
    });
    Ok(())
}

#[no_mangle]
pub extern "system" fn Java_com_bmshi_proxy_mobile_MainActivity_onPermissionResult<'local>(
    _: JNIEnv<'local>,
    _: JObject<'local>,
    granted: jboolean,
) {
    log::info!("onPermissionResult:{}", granted);
    if let Err(err) = crate::emit_event("on_permission_result", granted != 0) {
        log::error!("onPermissionResult failed:{:?}", err);
    }
}

#[no_mangle]
pub extern "system" fn Java_com_bmshi_proxy_mobile_TrojanProxy_onStart<'local>(
    _: JNIEnv<'local>,
    _: JObject<'local>,
    fd: jint,
) {
    if let Err(err) = on_vpn_start(fd) {
        log::error!("onStart failed:{:?}", err);
    }
}

fn on_vpn_start(fd: i32) -> Result<(), types::VpnError> {
    let (context, lock) = get_mut_context()?;
    context.fd = fd;
    context.running = Arc::new(AtomicBool::new(true));
    let running = context.running.clone();
    drop(lock);
    std::thread::spawn(move || {
        if let Err(err) = crate::process_vpn(fd, running) {
            log::error!("found error:{:?} while process vpn", err);
            if let Err(err) = emit_event("on_status_changed", 2) {
                log::error!("emit status changed failed:{:?}", err);
            }
        } else {
            log::warn!("vpn process exits");
        }
    });
    emit_event("on_status_changed", 1)
}

#[no_mangle]
pub extern "system" fn Java_com_bmshi_proxy_mobile_TrojanProxy_onStop<'local>(
    _: JNIEnv<'local>,
    _: JObject<'local>,
) {
    if let Err(err) = on_vpn_stop() {
        log::error!("call onStop failed:{:?}", err);
    }
}

fn on_vpn_stop() -> Result<(), types::VpnError> {
    let (context, lock) = get_mut_context()?;
    context.fd = -1;
    context.running.store(false, Ordering::Relaxed);
    drop(lock);
    emit_event("on_status_changed", 3)
}

pub fn init_log(log_level: &String) {
    let config = android_logger::Config::default();
    let config = match log_level.as_str() {
        "Trace" | "0" => config.with_max_level(log::LevelFilter::Trace),
        "Debug" | "1" => config.with_max_level(log::LevelFilter::Debug),
        "Info" | "2" => config.with_max_level(log::LevelFilter::Info),
        "Warn" | "3" => config.with_max_level(log::LevelFilter::Warn),
        "Error" | "4" => config.with_max_level(log::LevelFilter::Error),
        _ => config.with_max_level(log::LevelFilter::Debug),
    };
    let config = config.with_tag("VPN").format(|f, record| {
        write!(
            f,
            "[{}:{}][{}]{}",
            record.file().unwrap_or("unknown"),
            record.line().unwrap_or(0),
            record.level(),
            record.args()
        )
    });
    android_logger::init_once(config);
}

pub fn start_vpn(mtu: i32) -> Result<(), types::VpnError> {
    log::info!("start vpn proxy");
    let (context, lock) = get_context()?;
    let mut env = context.jvm.attach_current_thread()?;
    drop(lock);
    env.call_static_method(
        "com/bmshi/proxy/mobile/MainActivity",
        "startVpn",
        "(I)V",
        &[mtu.into()],
    )?;
    Ok(())
}

pub fn stop_vpn() -> Result<(), types::VpnError> {
    log::info!("stop vpn proxy");
    let (context, lock) = get_context()?;
    let mut env = context.jvm.attach_current_thread()?;
    drop(lock);
    env.call_static_method("com/bmshi/proxy/mobile/MainActivity", "stopVpn", "()V", &[])?;
    Ok(())
}

pub fn check_self_permission(permission: impl AsRef<str>) -> Result<bool, types::VpnError> {
    log::info!("check self permission:{}", permission.as_ref());
    let (context, lock) = get_context()?;
    let mut env = context.jvm.attach_current_thread()?;
    drop(lock);
    let permission = env.new_string(permission)?;
    let ret = env.call_static_method(
        "com/bmshi/proxy/mobile/MainActivity",
        "checkSelfPermission",
        "(Ljava/lang/String;)Z",
        &[(&permission).into()],
    )?;
    Ok(ret.z()?)
}

pub fn request_permission(permission: impl AsRef<str>) -> Result<(), types::VpnError> {
    log::info!("request permission:{}", permission.as_ref());
    let (context, lock) = get_context()?;
    let mut env = context.jvm.attach_current_thread()?;
    drop(lock);
    let permission = env.new_string(permission)?;
    env.call_static_method(
        "com/bmshi/proxy/mobile/MainActivity",
        "requestPermission",
        "(Ljava/lang/String;)V",
        &[(&permission).into()],
    )?;
    Ok(())
}

pub fn should_show_permission_rationale(
    permission: impl AsRef<str>,
) -> Result<bool, types::VpnError> {
    log::info!("should show permission rationale:{}", permission.as_ref());
    let (context, lock) = get_context()?;
    let mut env = context.jvm.attach_current_thread()?;
    drop(lock);
    let permission = env.new_string(permission)?;
    let ret = env.call_static_method(
        "com/bmshi/proxy/mobile/MainActivity",
        "shouldShowRequestPermissionRationaleNative",
        "(Ljava/lang/String;)Z",
        &[(&permission).into()],
    )?;
    Ok(ret.z()?)
}

pub fn update_notification(content: impl AsRef<str>) -> Result<(), types::VpnError> {
    log::info!("update notification:{}", content.as_ref());
    let (context, lock) = get_context()?;
    let mut env = context.jvm.attach_current_thread()?;
    drop(lock);
    let content = env.new_string(content)?;
    env.call_static_method(
        "com/bmshi/proxy/mobile/MainActivity",
        "updateNotification",
        "(Ljava/lang/String;)V",
        &[(&content).into()],
    )?;
    Ok(())
}

pub fn save_data(key: impl AsRef<str>, content: impl AsRef<str>) -> Result<(), types::VpnError> {
    log::info!("save data:{} - {}", key.as_ref(), content.as_ref());
    let (context, lock) = get_context()?;
    let mut env = context.jvm.attach_current_thread()?;
    drop(lock);
    let content = env.new_string(content)?;
    let key = env.new_string(key)?;
    env.call_static_method(
        "com/bmshi/proxy/mobile/MainActivity",
        "saveData",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        &[(&key).into(), (&content).into()],
    )?;
    Ok(())
}

pub fn load_data(key: impl AsRef<str>) -> Result<String, types::VpnError> {
    log::info!("load data:{}", key.as_ref());
    let (context, lock) = get_context()?;
    let mut env = context.jvm.attach_current_thread()?;
    drop(lock);
    let key = env.new_string(key)?;
    let ret = env.call_static_method(
        "com/bmshi/proxy/mobile/MainActivity",
        "loadData",
        "(Ljava/lang/String;)Ljava/lang/String;",
        &[(&key).into()],
    )?;

    let value: JString = ret.l()?.into();
    let value = env.get_string(&value)?.to_string_lossy().to_string();

    Ok(value)
}

pub struct Session {
    file: Mutex<ManuallyDrop<File>>,
    mtu: usize,
    show_info: bool,
}

pub struct Packet {
    data: Vec<u8>,
}

impl Session {
    pub fn new(fd: i32, mtu: usize, show_info: bool) -> Self {
        unsafe {
            let fd = OwnedFd::from_raw_fd(fd);
            let file = fd.into();
            Self {
                file: Mutex::new(ManuallyDrop::new(file)),
                mtu,
                show_info,
            }
        }
    }

    pub fn try_receive(&self) -> Result<Option<Packet>, ()> {
        let mut packet = Packet::new(self.mtu as u16);
        if let Ok(mut file) = self.file.lock() {
            match file.read(packet.bytes_mut()) {
                Ok(0) => {
                    log::error!("end of file");
                    Err(())
                }
                Ok(n) => {
                    packet.set_len(n);
                    Ok(Some(packet))
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => Ok(None),
                Err(err) => {
                    log::error!("read file failed:{:?}", err);
                    Err(())
                }
            }
        } else {
            Err(())
        }
    }

    pub fn allocate_send_packet(&self, size: u16) -> Result<Packet, ()> {
        Ok(Packet::new(size))
    }

    pub fn send_packet(&self, packet: Packet) {
        if let Ok(mut file) = self.file.lock() {
            if let Err(err) = file.write_all(packet.bytes()) {
                log::error!("send packet failed:{}", err);
            } else if self.show_info {
                if let Err(err) = packet.info() {
                    log::error!("parse return packet failed:{:?}", err);
                }
            }
        } else {
            log::error!("lock file for send packet failed");
        }
    }
}

impl Packet {
    pub fn new(size: u16) -> Self {
        let mut data = vec![0u8; size as usize];
        Self { data }
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        self.data.as_mut_slice()
    }

    pub fn bytes(&self) -> &[u8] {
        self.data.as_slice()
    }

    pub fn set_len(&mut self, len: usize) {
        unsafe {
            self.data.set_len(len);
        }
    }

    pub fn info(&self) -> types::Result<()> {
        let (dst_addr, src_addr, payload, protocol) = match IpVersion::of_packet(self.bytes())? {
            IpVersion::Ipv4 => {
                let packet = Ipv4Packet::new_checked(self.bytes())?;
                let dst_addr = packet.dst_addr();
                let src_addr = packet.src_addr();
                (
                    IpAddress::Ipv4(dst_addr),
                    IpAddress::Ipv4(src_addr),
                    packet.payload(),
                    packet.next_header(),
                )
            }
            IpVersion::Ipv6 => {
                let packet = Ipv6Packet::new_checked(self.bytes())?;
                let dst_addr = packet.dst_addr();
                let src_addr = packet.src_addr();
                (
                    IpAddress::Ipv6(dst_addr),
                    IpAddress::Ipv6(src_addr),
                    packet.payload(),
                    packet.next_header(),
                )
            }
        };
        let (dst_port, src_port, payload) = match protocol {
            IpProtocol::Udp => {
                let packet = UdpPacket::new_checked(payload)?;
                (packet.dst_port(), packet.src_port(), packet.payload())
            }
            IpProtocol::Tcp => {
                let packet = TcpPacket::new_checked(payload)?;
                (packet.dst_port(), packet.src_port(), packet.payload())
            }
            _ => return Ok(()),
        };
        log::info!(
            "send packet {} {}:{} - {}:{} {} bytes",
            protocol,
            src_addr,
            src_port,
            dst_addr,
            dst_port,
            payload.len()
        );
        Ok(())
    }
}

impl From<Vec<u8>> for Packet {
    fn from(value: Vec<u8>) -> Self {
        Self { data: value }
    }
}
