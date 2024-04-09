#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// hide console window on Windows in release
extern crate core;
extern crate csv;
extern crate preferences;
extern crate serde;

use std::cmp::max;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc, RwLock};
use std::thread;
use std::time::Duration;

use eframe::egui::{vec2, ViewportBuilder, Visuals};
use eframe::{egui, icon_data};
use gui::{GuiWindows, PlotOptions, RawTrafficOptions};
use preferences::AppInfo;

use crate::data::{DataContainer, Packet};
use crate::gui::{load_gui_settings, print_to_console, MyApp, Print, RIGHT_PANEL_WIDTH};
use crate::io::{save_to_csv, FileOptions};
use crate::record::{record_thread, RecordData, RecordOptions};
use crate::serial::{load_serial_settings, serial_thread, Device};

mod data;
mod gui;
mod io;
mod record;
mod serial;
mod toggle;

const APP_INFO: AppInfo = AppInfo {
    name: "Serial Monitor",
    author: "Linus Leo Stöckli",
};
const PREFS_KEY: &str = "config/gui";
const PREFS_KEY_SERIAL: &str = "config/serial_devices";

enum GuiEvent {
    SetRawTrafficOptions(RawTrafficOptions),
    SetBufferSize(usize),
    SetNames(Vec<String>),
    SaveCSV(FileOptions),
    SetGuiWindow(GuiWindows),
    Clear,
}

#[derive(Debug)]
enum QCMEvent {
    BiasDetectStart,
    BiasResult(i32),
    PhaseBaseDetectStart,
    PhaseBaseResult(f64),
    ShotIQStart(usize),
    ShotIQFinish(usize),
    RealtimeIQStart(usize),
    RealtimeIQFinish(usize),
    TrackStart(usize),
    MultiParamsStart(usize),
}

fn split(payload: &str) -> Vec<f64> {
    let mut split_data: Vec<&str> = vec![];
    for s in payload.split(':') {
        split_data.extend(s.split(','));
    }
    split_data
        .iter()
        .map(|x| x.trim())
        .flat_map(|x| x.parse::<f64>())
        .collect()
}

fn parse_qcm_event(cmd_strs: Vec<&str>) -> Option<QCMEvent> {
    let event_str = cmd_strs[0];
    match event_str {
        "BIASST" => Some(QCMEvent::BiasDetectStart),
        "BIAS" => Some(QCMEvent::BiasResult(cmd_strs.get(1)?.trim().parse().ok()?)),
        "PHAST" => Some(QCMEvent::PhaseBaseDetectStart),
        "PHABASE" => Some(QCMEvent::PhaseBaseResult(
            cmd_strs.get(1)?.trim().parse().ok()?,
        )),
        "SHOTST" => Some(QCMEvent::ShotIQStart(cmd_strs.get(1)?.trim().parse().ok()?)),
        "SHOTFIN" => Some(QCMEvent::ShotIQFinish(
            cmd_strs.get(1)?.trim().parse().ok()?,
        )),
        "RTST" => Some(QCMEvent::RealtimeIQStart(
            cmd_strs.get(1)?.trim().parse().ok()?,
        )),
        "RTFIN" => Some(QCMEvent::RealtimeIQFinish(
            cmd_strs.get(1)?.trim().parse().ok()?,
        )),
        "TRACKST" => Some(QCMEvent::TrackStart(cmd_strs.get(1)?.trim().parse().ok()?)),
        "MULPARAST" => Some(QCMEvent::MultiParamsStart(
            cmd_strs.get(1)?.trim().parse().ok()?,
        )),
        _ => None,
    }
}

fn main_thread(
    data_lock: Arc<RwLock<DataContainer>>,
    print_lock: Arc<RwLock<Vec<Print>>>,
    raw_data_rx: Receiver<Packet>,
    gui_event_rx: Receiver<GuiEvent>,
    record_data_tx: Sender<RecordData>,
    qcm_event_tx: Sender<QCMEvent>,
) {
    // reads data from mutex, samples and saves if needed
    // let mut data = DataContainer::default();
    let mut raw_traffic_options = RawTrafficOptions::default();
    let mut failed_format_counter = 0;
    let mut buffer_size = PlotOptions::default().buffer_size;
    let mut gui_window = GuiWindows::RawUART;

    loop {
        if let Ok(event) = gui_event_rx.try_recv() {
            match event {
                GuiEvent::SetRawTrafficOptions(opt) => raw_traffic_options = opt,
                GuiEvent::SetNames(names) => {
                    if let Ok(mut write_guard) = data_lock.write() {
                        write_guard.names = names;
                    }
                }
                GuiEvent::SaveCSV(csv_options) => {
                    if let Ok(read_guard) = data_lock.read() {
                        match save_to_csv(&read_guard, &csv_options) {
                            Ok(_) => {
                                print_to_console(
                                    &print_lock,
                                    Print::Ok(format!(
                                        "saved data file to {:?} ",
                                        csv_options.file_path
                                    )),
                                );
                            }
                            Err(e) => {
                                print_to_console(
                                    &print_lock,
                                    Print::Error(format!(
                                        "failed to save file to {:?}: {:?}",
                                        csv_options.file_path, e
                                    )),
                                );
                            }
                        }
                    }
                }
                GuiEvent::Clear => {
                    if let Ok(mut write_guard) = data_lock.write() {
                        *write_guard = DataContainer::default();
                        failed_format_counter = 0;
                    }
                }
                GuiEvent::SetBufferSize(s) => buffer_size = s,
                GuiEvent::SetGuiWindow(window) => gui_window = window,
            }
        }

        if let Ok(packet) = raw_data_rx.try_recv() {
            if !packet.payload.is_empty() {
                if packet.payload.starts_with("#") {
                    print_to_console(&print_lock, Print::Debug(packet.payload[1..].into()));
                    continue;
                }
                if packet.payload.starts_with("$") {
                    if gui_window == GuiWindows::RawUART {
                        continue;
                    }
                    let cmd_strs = packet.payload[1..].split("$").collect::<Vec<&str>>();
                    if let Some(event) = parse_qcm_event(cmd_strs) {
                        if let Ok(write_guard) = data_lock.write() {
                            let mut data = write_guard;
                            match event {
                                QCMEvent::BiasDetectStart => {
                                    data.names = vec!["Cur. Bias".into(), "Avg. Bias".into()];
                                    failed_format_counter = 20;
                                }
                                QCMEvent::PhaseBaseDetectStart => {
                                    data.names = vec![
                                        "Cur. Phase".into(),
                                        "Avg. Phase".into(),
                                        "Cur. Amp".into(),
                                    ];
                                    failed_format_counter = 20;
                                }
                                QCMEvent::ShotIQStart(_) => {
                                    data.names =
                                        vec!["Freq.".into(), "G Resp.".into(), "B Resp.".into()];
                                    failed_format_counter = 20;
                                }
                                QCMEvent::RealtimeIQStart(_) => {
                                    data.names = vec!["Freq.".into(), "Resp.".into()];
                                    failed_format_counter = 20;
                                }
                                QCMEvent::TrackStart(_) => {
                                    data.names =
                                        vec!["Cur. Reson. Freq.".into(), "Cur. B Resp.".into()];
                                    failed_format_counter = 20;
                                }
                                QCMEvent::MultiParamsStart(_) => {
                                    data.names = vec![
                                        "Cur. Reson. Freq.".into(),
                                        "Max. G Resp.".into(),
                                        "Q Factor".into(),
                                    ];
                                    failed_format_counter = 20;
                                }
                                _ => {}
                            }
                        }
                        qcm_event_tx.send(event).expect("failed to send qcm event");
                    }
                    continue;
                }
                if let Ok(write_guard) = data_lock.write() {
                    let mut data = write_guard;
                    if raw_traffic_options.enable {
                        data.raw_traffic.push(packet.clone());
                        let raw_traffic_len = data.raw_traffic.len();
                        data.raw_traffic = data
                            .raw_traffic
                            .split_off(raw_traffic_len.saturating_sub(raw_traffic_options.max_len));
                    }
                    let split_data = split(&packet.payload);

                    if data.dataset.is_empty()
                        || failed_format_counter > 10
                        || data.dataset[0].len() != data.time.len()
                    {
                        // resetting dataset
                        data.time = vec![];
                        data.absolute_time = vec![];
                        data.dataset = vec![vec![]; max(split_data.len(), 1)];
                        if data.names.len() != split_data.len() {
                            data.names = (0..max(split_data.len(), 1))
                                .map(|i| format!("Column {i}"))
                                .collect();
                        }
                        failed_format_counter = 0;
                        // println!("resetting dataset. split length = {}, length data.dataset = {}", split_data.len(), data.dataset.len());
                    } else if split_data.len() == data.dataset.len() {
                        record_data_tx
                            .send(RecordData {
                                time: packet.absolute_time,
                                datas: split_data.clone(),
                            })
                            .unwrap_or_default();
                        // appending data
                        for (i, set) in data.dataset.iter_mut().enumerate() {
                            set.push(split_data[i]);
                            failed_format_counter = 0;
                            while set.len() > buffer_size {
                                set.remove(0);
                            }
                        }
                        data.time.push(packet.relative_time);
                        while data.time.len() > buffer_size {
                            data.time.remove(0);
                        }
                        data.absolute_time.push(packet.absolute_time);
                        while data.absolute_time.len() > buffer_size {
                            data.absolute_time.remove(0);
                        }
                    } else {
                        // not same length
                        failed_format_counter += 1;
                        // println!("not same length in main! length split_data = {}, length data.dataset = {}", split_data.len(), data.dataset.len())
                    }
                }

                // if let Ok(mut write_guard) = data_lock.write() {
                //     *write_guard = data.clone();
                // }
            }
        }

        std::thread::sleep(Duration::from_millis(1));
    }
}

fn main() {
    let gui_settings = load_gui_settings();
    let saved_serial_device_configs = load_serial_settings();

    let device_lock = Arc::new(RwLock::new(Device::default()));
    let devices_lock = Arc::new(RwLock::new(vec![gui_settings.device.clone()]));
    let data_lock = Arc::new(RwLock::new(DataContainer::default()));
    let print_lock = Arc::new(RwLock::new(vec![Print::Empty]));
    let connected_lock = Arc::new(RwLock::new(false));

    let (send_tx, send_rx): (Sender<String>, Receiver<String>) = mpsc::channel();
    let (raw_data_tx, raw_data_rx): (Sender<Packet>, Receiver<Packet>) = mpsc::channel();
    let (gui_event_tx, gui_event_rx) = mpsc::channel::<GuiEvent>();
    let (record_options_tx, record_options_rx) = mpsc::channel::<RecordOptions>();
    let (record_data_tx, record_data_rx) = mpsc::channel::<RecordData>();
    let (qcm_event_tx, qcm_event_rx) = mpsc::channel::<QCMEvent>();

    let serial_device_lock = device_lock.clone();
    let serial_devices_lock = devices_lock.clone();
    let serial_print_lock = print_lock.clone();
    let serial_connected_lock = connected_lock.clone();

    println!("starting connection thread..");
    let _serial_thread_handler = thread::spawn(|| {
        serial_thread(
            send_rx,
            raw_data_tx,
            serial_device_lock,
            serial_devices_lock,
            serial_print_lock,
            serial_connected_lock,
        );
    });

    let record_data_lock = data_lock.clone();
    let record_print_lock = print_lock.clone();

    let _record_thread_handler = thread::spawn(|| {
        record_thread(
            record_data_lock,
            record_print_lock,
            record_options_rx,
            record_data_rx,
        )
    });

    let main_data_lock = data_lock.clone();
    let main_print_lock = print_lock.clone();

    println!("starting main thread..");
    let _main_thread_handler = thread::spawn(|| {
        main_thread(
            main_data_lock,
            main_print_lock,
            raw_data_rx,
            gui_event_rx,
            record_data_tx,
            qcm_event_tx,
        );
    });

    let options = eframe::NativeOptions {
        follow_system_theme: true,
        viewport: ViewportBuilder::default()
            .with_drag_and_drop(true)
            .with_inner_size(vec2(gui_settings.x, gui_settings.y))
            .with_min_inner_size(vec2(2.0 * RIGHT_PANEL_WIDTH, 2.0 * RIGHT_PANEL_WIDTH))
            .with_icon(
                icon_data::from_png_bytes(&include_bytes!("../icons/icon.png")[..]).unwrap(),
            ),
        ..Default::default()
    };

    let gui_data_lock = data_lock;
    let gui_device_lock = device_lock;
    let gui_devices_lock = devices_lock;
    let gui_connected_lock = connected_lock;
    let gui_print_lock = print_lock;

    if let Err(e) = eframe::run_native(
        "Serial Monitor",
        options,
        Box::new(|_cc| {
            let mut fonts = egui::FontDefinitions::default();

            egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);

            _cc.egui_ctx.set_fonts(fonts);
            _cc.egui_ctx.set_visuals(Visuals::light());

            Box::new(MyApp::new(
                gui_print_lock,
                gui_data_lock,
                gui_device_lock,
                gui_devices_lock,
                saved_serial_device_configs,
                gui_connected_lock,
                gui_settings,
                send_tx,
                gui_event_tx,
                record_options_tx,
                qcm_event_rx,
            ))
        }),
    ) {
        println!("error: {e:?}");
    }
}
