#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use backtrace::Backtrace;
use derive_more::From;
use serde::{Deserialize, Serialize};
use tauri::api::process::{Command, CommandChild, CommandEvent};
use tauri::{
    CustomMenuItem, Icon, Manager, RunEvent, State, SystemTray, SystemTrayEvent, SystemTrayMenu,
    SystemTrayMenuItem, Window, WindowEvent, Wry,
};
use tauri_plugin_log::LogTarget;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(From, Debug)]
pub enum Error {
    StdIo(std::io::Error),
    SerdeJson(serde_json::Error),
}

#[derive(Deserialize, Serialize, Debug, Default, Clone)]
pub struct Config {
    pub iface_name: String,
    pub server_domain: String,
    pub server_auth: String,
    pub default_dns: String,
    pub log_level: String,
    pub pool_size: u32,
    pub enable_ipset: bool,
    pub inverse_route: bool,
    pub enable_dns: bool,
    pub dns_listen: String,
    pub trust_dns: String,
}

impl Config {
    fn log_level_str(&self) -> &'static str {
        match self.log_level.as_str() {
            "Trace" => "0",
            "Debug" => "1",
            "Info" => "2",
            "Warn" => "3",
            "Error" => "4",
            _ => "5",
        }
    }
}

pub struct TrojanProxy {
    config: Config,
    wintun: Option<CommandChild>,
    dns: Option<CommandChild>,
    running_icon: Icon,
    stopped_icon: Icon,
}

impl TrojanProxy {
    fn new() -> TrojanProxy {
        TrojanProxy {
            config: init_config().unwrap_or_default(),
            wintun: None,
            dns: None,
            running_icon: Icon::Raw(include_bytes!("../icons/icon.ico").to_vec()),
            stopped_icon: Icon::Raw(include_bytes!("../icons/icon_gray.png").to_vec()),
        }
    }
}

type TrojanState = Arc<Mutex<TrojanProxy>>;

#[tauri::command]
fn start(config: Config, state: State<TrojanState>, window: Window<Wry>) {
    log::info!("start trojan now");
    if let Err(err) = save_config(&config) {
        log::error!("save config failed:{:?}", err);
    } else {
        emit_state_update_event(true, window.clone());
        state.lock().unwrap().config = config;
        if state.lock().unwrap().wintun.is_some() {
            return;
        }
        let state = state.inner().clone();
        tauri::async_runtime::spawn(async move {
            let config = state.lock().unwrap().config.clone();
            let default_dns = config.default_dns.clone() + ":53";
            let pool_size = config.pool_size.to_string();
            let config_ipset = window
                .app_handle()
                .path_resolver()
                .resolve_resource("config/ipset.txt")
                .unwrap();
            let config_wintun = window
                .app_handle()
                .path_resolver()
                .resolve_resource("libs/wintun.dll")
                .unwrap();
            let mut args = vec![
                "-l",
                "logs\\wintun.log",
                "-L",
                config.log_level_str(),
                "-a",
                "127.0.0.1:60080",
                "-p",
                config.server_auth.as_str(),
                "wintun",
                "-n",
                config.iface_name.as_str(),
                "-H",
                config.server_domain.as_str(),
                "--dns-server-addr",
                default_dns.as_str(),
                "-P",
                pool_size.as_str(),
                "-w",
                config_wintun.to_str().unwrap(),
            ];
            if config.enable_ipset {
                args.push("--route-ipset");
                args.push(config_ipset.to_str().unwrap());
                if config.inverse_route {
                    args.push("--inverse-route");
                }
            }
            let mut rxs = HashMap::new();
            match Command::new_sidecar("trojan").unwrap().args(args).spawn() {
                Ok((rx, child)) => {
                    state.lock().unwrap().wintun.replace(child);
                    rxs.insert("wintun", rx);
                }
                Err(err) => {
                    log::error!("start wintun failed:{:?}", err);
                    emit_state_update_event(false, window);
                    return;
                }
            };
            if state.lock().unwrap().config.enable_dns {
                tokio::time::sleep(Duration::from_secs(10)).await;
                let dns_listen = config.dns_listen.clone() + ":53";
                let config_domains = window
                    .app_handle()
                    .path_resolver()
                    .resolve_resource("config/domain.txt")
                    .unwrap();
                let mut args = vec![
                    "-l",
                    "logs\\dns.log",
                    "-L",
                    config.log_level_str(),
                    "-a",
                    "127.0.0.1:60080",
                    "-p",
                    config.server_auth.as_str(),
                    "dns",
                    "-n",
                    config.iface_name.as_str(),
                    "--blocked-domain-list",
                    config_domains.to_str().unwrap(),
                    "--poisoned-dns",
                    config.default_dns.as_str(),
                    "--dns-listen-address",
                    dns_listen.as_str(),
                ];
                if !config.enable_ipset {
                    args.push("--add-route");
                }
                match Command::new_sidecar("trojan").unwrap().args(args).spawn() {
                    Ok((rx, child)) => {
                        state.lock().unwrap().dns.replace(child);
                        rxs.insert("dns", rx);
                    }
                    Err(err) => {
                        log::error!("start dns failed:{:?}", err);
                        if let Some(wintun) = state.lock().unwrap().wintun.take() {
                            let _ = wintun.kill();
                        }
                    }
                }
            }
            log::info!("sub process started");

            while !rxs.is_empty() {
                let exited: Vec<_> = rxs
                    .iter_mut()
                    .filter_map(|(name, rx)| {
                        let exit = match rx.try_recv() {
                            Ok(CommandEvent::Terminated(payload)) => {
                                log::info!("{} exits with:{:?}", name, payload);
                                true
                            }
                            Ok(CommandEvent::Error(err)) => {
                                log::info!("{} got error:{}", name, err);
                                false
                            }
                            Ok(CommandEvent::Stderr(err)) => {
                                log::info!("{} got stderr:{}", name, err);
                                false
                            }
                            Ok(CommandEvent::Stdout(output)) => {
                                log::info!("{} got stdout:{}", name, output);
                                false
                            }
                            Err(_err) => false,
                            Ok(_) => false,
                        };
                        if exit {
                            let mut state = state.lock().unwrap();
                            match *name {
                                "wintun" => {
                                    state.wintun.take();
                                    if let Some(child) = state.dns.take() {
                                        let _ = child.kill();
                                    }
                                }
                                "dns" => {
                                    wintool::adapter::set_dns_server("".to_string());
                                    state.dns.take();
                                    if let Some(child) = state.wintun.take() {
                                        let _ = child.kill();
                                    }
                                }
                                _ => {
                                    log::error!("invalid name:{}", name);
                                }
                            }
                            Some(name.to_string())
                        } else {
                            None
                        }
                    })
                    .collect();
                for name in exited {
                    rxs.remove(name.as_str());
                }
                tokio::time::sleep(Duration::from_millis(66)).await;
            }
            emit_state_update_event(false, window);
            log::info!("sub process exits");
        });
    }
}

fn emit_state_update_event(running: bool, window: Window<Wry>) {
    window.emit("state-update", running).unwrap();
    let app = window.app_handle();
    let state = app.state::<TrojanState>();
    let state = state.lock().unwrap();
    let icon = if running {
        state.running_icon.clone()
    } else {
        state.stopped_icon.clone()
    };
    window.set_icon(icon.clone()).unwrap();
    window.app_handle().tray_handle().set_icon(icon).unwrap();
}

#[tauri::command]
fn init(state: State<TrojanState>) -> Config {
    state.lock().unwrap().config.clone()
}

#[tauri::command]
fn stop(state: State<TrojanState>, window: Window<Wry>) {
    log::info!("stop trojan now");
    let mut config = state.lock().unwrap();
    if let Some(child) = config.wintun.take() {
        let _ = child.kill();
        log::info!("trojan stopped");
    } else {
        emit_state_update_event(false, window);
    }
}

fn save_config(config: &Config) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open("config\\config.json")?;
    let data = serde_json::to_string(config)?;
    file.write_all(data.as_bytes())?;
    Ok(())
}

fn init_config() -> Result<Config> {
    let path = Path::new("config\\config.json");
    let config = if path.exists() {
        let file = File::open(path)?;
        let config: Config = serde_json::from_reader(file)?;
        config
    } else {
        Config::default()
    };
    Ok(config)
}

fn main() {
    let path = Path::new("logs");
    if !path.exists() {
        std::fs::create_dir(path).unwrap();
    }
    std::panic::set_hook(Box::new(|info| {
        let trace = Backtrace::new();
        let message = info.to_string();
        if let Ok(mut file) = OpenOptions::new()
            .write(true)
            .append(true)
            .create(true)
            .open("logs\\crash.log")
        {
            let _ = write!(
                &mut file,
                "[{}]client crash with error:{}\ntrace:{:?}\n",
                chrono::Local::now().format("[%Y-%m-%d %H:%M:%S%.6f]"),
                message,
                trace
            );
        }
    }));

    let quit = CustomMenuItem::new("quit".to_string(), "退出");
    let dev = CustomMenuItem::new("dev".to_string(), "开发工具");
    let menu = SystemTrayMenu::new();

    #[cfg(debug_assertions)]
    let menu = menu
        .add_item(dev)
        .add_native_item(SystemTrayMenuItem::Separator);
    let menu = menu.add_item(quit);
    let tray = SystemTray::new().with_menu(menu);

    let state = Arc::new(Mutex::new(TrojanProxy::new()));
    tauri::Builder::default()
        .manage(state.clone())
        .invoke_handler(tauri::generate_handler![start, init, stop])
        .system_tray(tray)
        .plugin(
            tauri_plugin_log::Builder::default()
                .targets([LogTarget::LogDir, LogTarget::Webview, LogTarget::Stdout])
                .format(|callback, args, record| {
                    callback.finish(format_args!(
                        "{}[{}:{}][{}]{}",
                        chrono::Local::now().format("[%Y-%m-%d %H:%M:%S%.6f]"),
                        record.file().unwrap_or("tauri"),
                        record.line().unwrap_or(0),
                        record.level(),
                        args
                    ))
                })
                .build(),
        )
        .plugin(tauri_plugin_single_instance::init(|app, args, cwd| {
            log::info!(
                "app:{}, args:{:?}, cwd:{}",
                app.package_info().name,
                args,
                cwd
            );
        }))
        .on_system_tray_event(|app, event| match event {
            SystemTrayEvent::MenuItemClick { id, .. } => match id.as_str() {
                "quit" => {
                    let state: State<TrojanState> = app.state();
                    let mut state = state.lock().unwrap();
                    if let Some(dns) = state.dns.take() {
                        wintool::adapter::set_dns_server("".to_string());
                        let _ = dns.kill();
                        thread::sleep(Duration::from_millis(500));
                    }
                    if let Some(wintun) = state.wintun.take() {
                        let _ = wintun.kill();
                    }
                    std::process::exit(0);
                }
                #[cfg(debug_assertions)]
                "dev" => {
                    let window = app.get_window("main").unwrap();
                    if !window.is_devtools_open() {
                        window.open_devtools();
                    }
                }
                _ => {}
            },
            SystemTrayEvent::DoubleClick { .. } => {
                let window = app.get_window("main").unwrap();
                window.show().unwrap();
                window.set_focus().unwrap();
            }
            _ => {}
        })
        .setup(|app| {
            emit_state_update_event(false, app.get_window("main").unwrap());
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while running tauri application")
        .run(|app, event| {
            if let RunEvent::WindowEvent {
                event: WindowEvent::CloseRequested { api, .. },
                ..
            } = event
            {
                app.get_window("main").unwrap().hide().unwrap();
                api.prevent_close();
            }
        });
}