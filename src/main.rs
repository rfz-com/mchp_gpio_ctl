use nusb::MaybeFuture;
use std::thread::sleep;
use std::time::Duration;

use clap::{Parser, Subcommand};
use colored::Colorize;
use mchp_gpio_ctl::dongle_hal_revc::{
    HeaderPin, PinMode, PinState, gpio_header_get, gpio_header_get_mode, gpio_header_set,
    gpio_header_set_mode, slg_io_get, slg_io_set, slg_io_set_mode, usb_switch_configure,
    usb_switch_set,
};
use mchp_gpio_ctl::{
    dongle_hal_revb::{
        PcbRevision, dev_power_ctl, is_dev_power_on, is_dev_pwr_fault, pcb_revision,
    },
    dongle_hal_revc::{SlgPin, usb_switch_is_connected},
};

const VENDOR_SMSC: u16 = 0x0424;
const PRODUCT_BRIDGE_DEV: u16 = 0x2530;
const PRODUCT_USB4604_HUB: u16 = 0x4502;

const VENDOR_FTDI: u16 = 0x0403;
const PRODUCT_FT234: u16 = 0x6015;

fn from_arg_usbpath(s: &str) -> Result<String, String> {
    <USBDevice as USBDeviceAPI>::from_arg(s)
        .map_err(|_| format!("unknown value: '{s}'."))
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    /// Serial number of a device to use, can use partial serial number if the result is unique, can not be used together with usbpath
    #[arg(short, long, group = "dongle-selection")]
    serial: Option<String>,
    /// Usb path of a device to use, not to be used with serial number selector
    #[arg(short, long, value_parser = from_arg_usbpath, group = "dongle-selection",
          value_name = "bus:port-chain")]
    usbpath: Option<String>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Power on if not already on
    On,
    /// Power off if not already off
    Off,
    /// Print dongle information (power status, IO config)
    Status,
    /// List connected devices serials
    List,

    // Only on RevC
    /// Force SDP for 10 seconds, then go back to USART mode, assuming switch is in USART mode (PCB RevC and up)
    Sdp,
    /// Force SDP mode (Amber LED will blink fast) (PCB RevC and up)
    ForceSdp,
    /// Release to USART mode (Amber LED will not blink, unless switch is in SDP mode) (PCB RevC and up)
    ReleaseSdp,

    /// Disconnect USB data lines from a device via hardware switch (PCB RevC and up)
    Detach,
    /// Connect USB data lines to the device (default) (PCB RevC and up)
    Attach,
    /// Emulate cable detach - disconnect USB data lines, set CC lines to low and disable power to a device (PCB RevC and up)
    FullDetach,
    /// Emulate cable insertion - reconnect USB data lines, set CC lines according to the switch position or force-sdp command, provide power (PCB RevC and up)
    FullAttach,

    /// Configure GPIO header pin (p0 or p1) as Input or Output (e.g., gpio-config p0 output) (PCB RevC and up)
    GpioConfig {
        pin: HeaderPin,
        mode: PinMode,
    },
    /// Set GPIO header pin configured as Output to High or Low (e.g., gpio-set p0 high) (PCB RevC and up)
    GpioSet {
        pin: HeaderPin,
        state: PinState,
    },
    /// Read GPIO header pin state (PCB RevC and up)
    GpioGet {
        pin: HeaderPin,
    },

    /// Print udev rule to the stdout, run 'mchp_gpio_ctl udev --help' for more information
    ///
    /// Create udev rule:
    /// mchp_gpio_ctl udev | sudo tee /etc/udev/rules.d/70-rm_dongle.rules
    ///
    /// Reload rules and trigger:
    /// sudo udevadm control --reload-rules
    /// sudo udevadm trigger
    #[cfg(target_os = "linux")]
    #[command(verbatim_doc_comment)]
    Udev,
}

#[derive(Debug, Clone)]
struct USBDevice {
    device_info: DeviceInfo,
    serial: String,
    product_string: String,
}

#[derive(Debug, PartialEq)]
pub struct USBDeviceError;

use nusb::DeviceInfo;

pub trait USBDeviceAPI {
    /// DeviceInfo
    fn device(&self) -> &DeviceInfo;
    /// Bus id as string
    fn bus_id(&self) -> &str;
    /// Port chain as vector of u8's
    fn port_chain(&self) -> &[u8];
    /// usb vendor id
    fn vendor(&self) -> u16;
    /// usb product id
    fn product(&self) -> u16;
    /// product string if available (exposed)
    fn product_string(&self) -> Option<String>;
    /// serial number if available (exposed)
    fn serial_number(&self) -> Option<String>;
    /// usb bus and port_chain as bus:p0,p2,...
    fn usb_path(&self) -> String;
    /// Compare usbpath to device bus_id and port_chain for match
    fn usbpath_match(&self, s: &str) -> bool;
    /// Compare serial to device serial for match
    fn serial_match(&self, s: &str) -> bool;

    // Helper functions
    /// from argument to usb_path if possible
    fn from_arg(s: &str) -> std::result::Result<String, USBDeviceError>;

    /// bus id representation santizing
    fn sanitize_usbpath(s: &str) -> String;
}

impl USBDeviceAPI for USBDevice {
    fn device(&self) -> &DeviceInfo {
        &self.device_info
    }

    fn bus_id(&self) -> &str {
        self.device_info.bus_id()
    }

    fn port_chain(&self) -> &[u8] {
        self.device_info.port_chain()
    }

    fn vendor(&self) -> u16 {
        self.device_info.vendor_id()
    }

    fn product(&self) -> u16 {
        self.device_info.product_id()
    }

    fn product_string(&self) -> Option<String> {
        self.device_info.product_string().map(|p| p.to_string())
    }

    fn serial_number(&self) -> Option<String> {
        self.device_info.serial_number().map(|p| p.to_string())
    }


    fn usb_path(&self) -> String {
        // Build the uuu usbpath from bus and port_chain
        let mut u_p: String = Self::sanitize_usbpath(self.bus_id()).to_string();
        u_p.push(':');
        for port in self.port_chain().iter() {
            u_p.push_str(&port.to_string());
        }
        u_p.clone()
    }

    fn usbpath_match(&self, s: &str) -> bool {
        let device_usb_path = self.usb_path();
        device_usb_path == s
    }

    fn serial_match(&self, s: &str) -> bool {
        match self.serial_number() {
            None => false,
            Some(device_serial) => device_serial == s
        }
    }

    fn from_arg(s: &str) -> std::result::Result<String, USBDeviceError> {
        use regex::Regex;
        // Note that we don't really know the spec for usb bus id, and
        // we assume that hub/port chains are limited to 7 wide per tier
        let re = Regex::new(r"\w+:[1-8]+").unwrap();
        if re.is_match(s) {
            Ok(s.to_string())
        } else {
            Err(USBDeviceError)
        }
    }

    fn sanitize_usbpath(usbpath: &str) -> String {
        match i64::from_str_radix(usbpath, 16) {
            Err(_e) => usbpath.to_string(), // Failed to parse, return str as String
            Ok(bus_id) => bus_id.to_string(), // Managed to parse, return bus in decimal
        }
    }
}

#[derive(thiserror::Error, Debug)]
/// Enum defining the errors in this module
pub enum DongleError {
    #[error("No dongle matching usb tree {}", .0)]
    /// No device matching usb tree
    NoDongleMathcingUSBTree(String),

    #[error("No dongle mathcing serial number {}", .0)]
    /// No device matching serial
    NoDongleMathcingSerial(String),

    #[error("No devices found")]
    /// No devices/dongles found
    NoDevicesFound,

    #[error("Serial {} matches more than one device", .0)]
    /// Serial matches more than one device
    SerialNotUnique(String),

    #[error("No match for serial {}", .0)]
    /// Serial does not match device(s)
    NoMatchForSerial(String),

    #[error("No match for USB tree {}", 0)]
    /// USB tree does not match device(s)
    NoMatchForUSBTree(String),

    #[error("Device not specified")]
    /// Several devices detected, need to specify which
    DeviceNotSpecified,

    #[error("Device open failed")]
    /// Device open failed
    DeviceOpenFail,

    #[error("ForceSDP not supported on PCB Rev A or B")]
    /// Force SDP not supported on Rev A or B
    ForceSDPNotSupported,

    #[error("Attch/Detach not supported on PCB Rev A or B")]
    /// Attch/Detach not supported on Rev A or B
    AttachDetachNotSupported,

    #[error("GPIO not supported on PCB Rev A or B")]
    /// GPIO not supported on Rev A or B
    GPIONotSupported,
}

fn main() -> Result<(), DongleError> {
    env_logger::init();
    let cli = Cli::parse();

    let all_devices = nusb::list_devices().wait().unwrap().collect::<Vec<_>>();
    // println!("Devices: {:#?}", all_devices);
    let devices = all_devices
        .iter()
        .filter(|d| d.vendor_id() == VENDOR_SMSC && d.product_id() == PRODUCT_BRIDGE_DEV)
        .map(|d| {
            let same_hub = d.port_chain();
            let same_hub = &same_hub[..same_hub.len() - 1];
            let ftdi = all_devices.iter().find(|d| {
                d.port_chain().starts_with(same_hub)
                    && d.vendor_id() == VENDOR_FTDI
                    && d.product_id() == PRODUCT_FT234
            });
            let serial = ftdi.and_then(|f| f.serial_number()).unwrap_or("");
            let hub = all_devices.iter().find(|d| {
                d.port_chain().starts_with(same_hub) && d.vendor_id() == VENDOR_SMSC && d.product_id() == PRODUCT_USB4604_HUB
            });
            let product_string = hub.and_then(|h| h.product_string()).unwrap_or("");
            USBDevice {
                device_info: d.clone(),
                serial: serial.to_string(),
                product_string: product_string.to_string()
            }
        })
        .collect::<Vec<_>>();
    // println!("{:?}", devices);

    // for usb_device in devices.iter() {
    //    println!("Dongle - serial: {} usb-path: {}", usb_device.serial, usb_device.usb_path());
    // }

    if matches!(cli.command, Commands::List) {
        println!("Connected device list:");
        for device in devices {
            println!("{} {}", device.serial, device.usb_path());
        }
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    if matches!(cli.command, Commands::Udev) {
        println!(
            r#"SUBSYSTEMS=="usb", ATTRS{{idVendor}}=="{VENDOR_SMSC:04x}", ATTRS{{idProduct}}=="{PRODUCT_BRIDGE_DEV:04x}", TAG+="uaccess", GROUP="plugdev", MODE="0660""#
        );
        println!(
            r#"SUBSYSTEMS=="usb", ATTRS{{idVendor}}=="{VENDOR_FTDI:04x}", ATTRS{{idProduct}}=="{PRODUCT_FT234:04x}", TAG+="uaccess", GROUP="plugdev", MODE="0660""#
        );
        return Ok(());
    }

    let d = if devices.is_empty() {
        println!("No devices found");
        return Err(DongleError::NoDevicesFound);
    } else if cli.serial.is_some() {
        let serial = cli.serial.unwrap();
        match devices.iter().find(|d| d.serial.contains(&serial)) {
            Some(d) => {
                let total_matches = devices
                    .iter()
                    .filter_map(|d| d.serial.contains(&serial).then_some(()))
                    .count();
                if total_matches == 1 {
                    &d.clone()
                } else {
                    println!("Devices found, but serial provided matches more than one device");
                    return Err(DongleError::SerialNotUnique(serial));
                }
            }
            None => {
                println!(
                    "Devices found, but serial provided does not match any of them, device serials:"
                );
                for d in devices {
                    println!("{}", d.serial);
                }
                return Err(DongleError::NoMatchForSerial(serial));
            }
        }
    } else if cli.usbpath.is_some() {
        let usbpath = cli.usbpath.unwrap();
        match devices.iter().find(|d| d.usbpath_match(usbpath.as_str())){
            Some(d) => {
                &d.clone()
            }
            None => {
                println!(
                    "Devices found, but usb path provided does not match any of them:"
                );
                for d in devices {
                    println!("{}", d.usb_path());
                }
                return Err(DongleError::NoMatchForUSBTree(usbpath));
            }
        }
    } else if devices.len() == 1 {
        match cli.serial {
            // DEAD CODE???
            Some(serial) => {
                if devices[0].serial.contains(&serial) {
                    &devices[0]
                } else {
                    println!(
                        "Devices found, but serial provided does not match any of them, device serials:"
                    );
                    for device in devices {
                        println!("{}", device.serial);
                    }
                    return Err(DongleError::NoMatchForSerial(serial));
                }
            }
            None => &devices[0].clone(),
        }
    } else {
        println!(
            "Several devices connected, please provide serial or usb path to select one of them"
        );
        for d in devices {
            println!("{} {}", d.serial, d.usb_path());
        }
        return Err(DongleError::DeviceNotSpecified);
    };

    let device = match d.device_info.open().wait() {
        Ok(d) => d,
        Err(e) => {
            println!("Failed to open device: {e}");
            #[cfg(target_os = "linux")]
            if e.kind() == nusb::ErrorKind::PermissionDenied || e.os_error() == Some(13) {
                println!(
                    "You are probably missing an udev rule, run 'mchp_gpio_ctl --help' to see how to install it"
                );
            }
            return Err(DongleError::DeviceOpenFail);
        }
    };
    let interface = device.claim_interface(0).wait().unwrap();

    let is_pwr_on = is_dev_power_on(&interface);
    let is_pwr_fault = is_dev_pwr_fault(&interface);
    if is_pwr_fault {
        println!("{}", "Power FAULT detected, probably short on VBUS?".red());
    }
    let pcb_revision = pcb_revision(&interface);
    // if matches!(pcb_revision, PcbRevision::RevC) {
    // println!("Detected PCB RevC");
    // setup_revc(&interface);
    // }
    let is_relay_variant = d.product_string.contains("relay");

    match &cli.command {
        Commands::On => {
            if is_pwr_on {
                println!("Power is already ON");
            } else {
                println!("Turning ON...");
                dev_power_ctl(&interface, true);
            }
        }
        Commands::Off => {
            if is_pwr_on {
                println!("Turning OFF...");
                dev_power_ctl(&interface, false);
            } else {
                println!("Power is already OFF");
            }
        }
        Commands::Status => {
            println!("Dongle serial: {}", d.serial);
            if is_pwr_on {
                println!("Power is ON");
            } else {
                println!("Power is OFF");
            }
            println!("PCB revision: {pcb_revision:?}");
            if is_relay_variant {
                println!("SSR (opto-relay) variant");
            }
            if matches!(pcb_revision, PcbRevision::RevC) {
                println!(
                    "USB switch connected: {}",
                    usb_switch_is_connected(&interface)
                );
                println!(
                    "Is forcing SDP mode: {:?}",
                    slg_io_get(&interface, SlgPin::SlgIo0) == PinState::High
                );
                println!(
                    "Is forcing CC lines down: {:?}",
                    slg_io_get(&interface, SlgPin::SlgIo1) == PinState::Low
                );
                if is_relay_variant {
                    let mode = gpio_header_get_mode(&interface, HeaderPin::P0);
                    if mode == PinMode::Input {
                        println!("{}", "Relay pin p0 is configured as Input, relay won't work".yellow());
                    } else {
                        let state = gpio_header_get(&interface, HeaderPin::P0);
                        if state == PinState::High {
                            println!("Relay state: Short (p0 high)");
                        } else {
                            println!("Relay state: Open (p0 low)");
                        }
                    }
                } else {
                    println!(
                        "Header pin 0 mode: {:?}, state: {:?}",
                        gpio_header_get_mode(&interface, HeaderPin::P0),
                        gpio_header_get(&interface, HeaderPin::P0)
                    );
                }
                println!(
                    "Header pin 1 mode: {:?}, state: {:?}",
                    gpio_header_get_mode(&interface, HeaderPin::P1),
                    gpio_header_get(&interface, HeaderPin::P1)
                );
            }
        }
        Commands::List => {}

        #[cfg(target_os = "linux")]
        Commands::Udev => {}

        Commands::ForceSdp | Commands::ReleaseSdp | Commands::Sdp => {
            if matches!(pcb_revision, PcbRevision::RevAorB) {
                println!("{}", "ForceSDP is not supported on PCB RevA or B".red());
                return Err(DongleError::ForceSDPNotSupported);
            }
            slg_io_set_mode(&interface, SlgPin::SlgIo0, PinMode::Output);
            match &cli.command {
                Commands::ForceSdp => {
                    slg_io_set(&interface, SlgPin::SlgIo0, PinState::High);
                }
                Commands::ReleaseSdp => {
                    slg_io_set(&interface, SlgPin::SlgIo0, PinState::Low);
                }
                Commands::Sdp => {
                    slg_io_set(&interface, SlgPin::SlgIo0, PinState::High);
                    for i in (1..=10).rev() {
                        println!("{i}");
                        sleep(Duration::from_secs(1));
                    }
                    slg_io_set(&interface, SlgPin::SlgIo0, PinState::Low);
                }
                _ => {}
            }
        }

        Commands::Attach | Commands::Detach => {
            if matches!(pcb_revision, PcbRevision::RevAorB) {
                println!(
                    "{}",
                    "Attach / Detach is not supported on PCB RevA or B".red()
                );
                return Err(DongleError::AttachDetachNotSupported);
            }
            usb_switch_configure(&interface);
            match &cli.command {
                Commands::Attach => {
                    usb_switch_set(&interface, true);
                }
                Commands::Detach => {
                    usb_switch_set(&interface, false);
                }
                _ => {}
            }
        }

        Commands::FullAttach | Commands::FullDetach => {
            if matches!(pcb_revision, PcbRevision::RevAorB) {
                println!(
                    "{}",
                    "Full Attach / Detach is not supported on PCB RevA or B".red()
                );
                return Err(DongleError::AttachDetachNotSupported);
            }
            usb_switch_configure(&interface);
            slg_io_set_mode(&interface, SlgPin::SlgIo1, PinMode::Output);
            match &cli.command {
                Commands::FullAttach => {
                    dev_power_ctl(&interface, true);
                    usb_switch_set(&interface, true);
                    slg_io_set(&interface, SlgPin::SlgIo1, PinState::High);
                }
                Commands::FullDetach => {
                    dev_power_ctl(&interface, false);
                    usb_switch_set(&interface, false);
                    slg_io_set(&interface, SlgPin::SlgIo1, PinState::Low);
                }
                _ => {}
            }
        }

        Commands::GpioConfig { .. } | Commands::GpioSet { .. } | Commands::GpioGet { .. } => {
            if matches!(pcb_revision, PcbRevision::RevAorB) {
                println!("{}", "GPIO is not supported on PCB RevA or B".red());
                return Err(DongleError::GPIONotSupported);
            }
            match &cli.command {
                Commands::GpioConfig { pin, mode } => {
                    if is_relay_variant && *pin == HeaderPin::P0 && *mode == PinMode::Input {
                        println!("{}", "Configuring relay control pin as input, relay won't work".yellow());
                    }
                    gpio_header_set_mode(&interface, *pin, *mode);
                }
                Commands::GpioSet { pin, state } => {
                    gpio_header_set(&interface, *pin, *state);
                }
                Commands::GpioGet { pin } => {
                    let state = gpio_header_get(&interface, *pin);
                    println!("{pin:?} = {state:?}");
                }
                _ => {}
            }
        }

    }
    Ok(())
}
