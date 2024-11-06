use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::exit;
use std::thread;
use std::time::Duration;
use serde::Deserialize;
use once_cell::sync::Lazy;
use std::sync::Mutex;

use influxdb2::models::DataPoint;
use influxdb2::Client;
use futures::stream;

const AMD_MSR_PWR_UNIT: u64 = 0xC0010299;
const AMD_MSR_CORE_ENERGY: u64 = 0xC001029A;
const AMD_MSR_PACKAGE_ENERGY: u64 = 0xC001029B;
const AMD_ENERGY_UNIT_MASK: u64 = 0x1F00;

const MAX_CPUS: usize = 1024;
const MAX_PACKAGES: usize = 16;

const RYZENMON_CONFIG_DIR: &str = "/etc/ryzenmon";
const RYZENMON_CONFIG_PATH: &str = "/etc/ryzenmon/config.toml";
// Configuration has: influxdb host, org, token, bucket


#[derive(Deserialize, Debug, Default)]
struct Config {
    influxdb: InfluxDBConfig,
}

#[derive(Deserialize, Debug, Default)]
struct InfluxDBConfig {
    host: String,
    org: String,
    token: String,
    bucket: String,
}

static CONFIG: Lazy<Mutex<Config>> = Lazy::new(|| Mutex::new(Config::default()));

fn load_config() -> Result<Config, Box<dyn std::error::Error>> {
    if !Path::new(RYZENMON_CONFIG_PATH).exists() {
        fs::create_dir_all(RYZENMON_CONFIG_DIR)?;

        let example_config = r#"
[influxdb]
host = "http://localhost:8086"
org = "your_org"
token = "your_token"
bucket = "your_bucket"
"#;
        let mut file = fs::File::create(RYZENMON_CONFIG_PATH)?;
        file.write_all(example_config.as_bytes())?;
        println!("Created example config at {}", RYZENMON_CONFIG_PATH);
        exit(1);
    }

    let config_content = fs::read_to_string(RYZENMON_CONFIG_PATH)?;
    let config: Config = toml::from_str(&config_content)?;
    Ok(config)
}

#[derive(Debug)]
pub struct PowerMetrics {
    core_watts: Vec<f64>,
    core_sum: f64,
    package_watts: f64,
}

fn detect_packages() -> io::Result<usize> {
    let mut package_map = vec![-1; MAX_PACKAGES];
    let mut total_cores = 0;

    for i in 0..MAX_CPUS {
        let filename = format!("/sys/devices/system/cpu/cpu{}/topology/physical_package_id", i);
        if let Ok(contents) = std::fs::read_to_string(&filename) {
            let package: i32 = contents.trim().parse().unwrap_or(-1);
            if package_map[package as usize] == -1 {
                package_map[package as usize] = i as i32;
            }
            total_cores = i + 1;
        } else {
            break;
        }
    }

    Ok(total_cores)
}

fn open_msr(core: usize) -> io::Result<File> {
    let msr_filename = format!("/dev/cpu/{}/msr", core);
    OpenOptions::new()
        .read(true)
        .open(&msr_filename)
        .map_err(|e| {
            eprintln!("Failed to open MSR for core {}: {}", core, e);
            e
        })
}

fn read_msr(file: &mut File, which: u64) -> io::Result<i64> {
    let mut buffer = [0u8; 8];
    file.seek(SeekFrom::Start(which))?;
    file.read_exact(&mut buffer)?;
    Ok(i64::from_ne_bytes(buffer))
}

fn rapl_msr_amd_core(total_cores: usize) -> io::Result<PowerMetrics> {
    let mut core_energy = vec![0.0; total_cores/2];
    let mut core_energy_delta = vec![0.0; total_cores/2];
    let mut package = vec![0.0; total_cores/2];
    let mut package_delta = vec![0.0; total_cores/2];
    let mut files: Vec<File> = Vec::new();

    for i in 0..total_cores/2 {
        files.push(open_msr(i)?);
    }

    let core_energy_units = read_msr(&mut files[0], AMD_MSR_PWR_UNIT)? as u64;
    let energy_unit = (core_energy_units & AMD_ENERGY_UNIT_MASK) >> 8;
    let energy_unit_d = 0.5f64.powf(energy_unit as f64);

    for i in 0..total_cores/2 {
        let core_energy_raw = read_msr(&mut files[i], AMD_MSR_CORE_ENERGY)? as f64;
        let package_raw = read_msr(&mut files[i], AMD_MSR_PACKAGE_ENERGY)? as f64;
        
        core_energy[i] = core_energy_raw * energy_unit_d;
        package[i] = package_raw * energy_unit_d;
    }

    thread::sleep(Duration::from_micros(100000));

    for i in 0..total_cores/2 {
        let core_energy_raw = read_msr(&mut files[i], AMD_MSR_CORE_ENERGY)? as f64;
        let package_raw = read_msr(&mut files[i], AMD_MSR_PACKAGE_ENERGY)? as f64;
        
        core_energy_delta[i] = core_energy_raw * energy_unit_d;
        package_delta[i] = package_raw * energy_unit_d;
    }

    let mut core_watts = Vec::with_capacity(total_cores/2);
    let mut sum = 0.0;
    let package_watts = (package_delta[0] - package[0]) * 10.0;

    for i in 0..total_cores/2 {
        let watts = (core_energy_delta[i] - core_energy[i]) * 10.0;
        core_watts.push(watts);
        sum += watts;
    }

    Ok(PowerMetrics {
        core_watts,
        core_sum: sum,
        package_watts,
    })
}

async fn upload(metrics : PowerMetrics) -> Result<(), Box<dyn std::error::Error>> {
    let config = CONFIG.lock().unwrap();
    let InfluxDBConfig { host, org, token, bucket } = &config.influxdb;
    let client = Client::new(host, org, token);

    let points = vec![
        DataPoint::builder("power")
            .tag("host", "pvehost")
            .tag("service", "ryzen-rapl")
            .field("core-power", metrics.core_sum)
            .build()?,
        DataPoint::builder("power")
            .tag("host", "pvehost")
            .tag("service", "ryzen-rapl")
            .field("package-power", metrics.package_watts)
            .build()?,
    ];

    client.write(bucket, stream::iter(points)).await?;
    Ok(())
}

async fn worker(total_cores: usize) -> Result<(), Box<dyn std::error::Error>> {
    let metrics = rapl_msr_amd_core(total_cores)?;

    if let Err(e) = upload(metrics).await {
        eprintln!("Upload failed: {}", e);
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = load_config()?;
    {
        let mut global_config = CONFIG.lock().unwrap();
        *global_config = config;
    }
    println!("Loaded config: {:?}", *CONFIG.lock().unwrap());

    let total_cores = detect_packages();

    let mut cores = 0;
    match total_cores {
        Ok(total_cores) => {
            println!("Detected {} cores", total_cores);
            cores = total_cores;
        },
        Err(e) => {
            eprintln!("Failed to detect cores: {}", e);
            return Ok(());
        }
    }

    loop {
        if let Err(e) = worker(cores).await {
            eprintln!("Worker failed: {}", e);
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}
