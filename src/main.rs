// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
// This product includes software developed at Datadog (https://www.datadoghq.com/). Copyright 2021 Datadog, Inc.

use anyhow::*;
use async_std::{
    net::UdpSocket,
    process::{Command, Stdio},
    sync::{Arc, Barrier, RwLock},
    task::{sleep, spawn},
};
use serde::Serialize;
use serde_json::json;
use std::{collections::HashMap, env, process::exit, time::Duration};

mod config;
use config::*;

mod rusage;
use rusage::*;

#[derive(Serialize, Clone)]
#[serde(untagged)]
enum MetricValue {
    Str(String),
    Num(f64),
    Arr(Vec<HashMap<String, MetricValue>>),
}

impl MetricValue {
    fn as_f64(self) -> f64 {
        match self {
            Self::Num(x) => x,
            _ => panic!("not an f64"),
        }
    }
}

impl From<String> for MetricValue {
    fn from(string: String) -> Self {
        MetricValue::Str(string)
    }
}

macro_rules! num_type {
    ($type:ty) => {
        impl From<$type> for MetricValue {
            fn from(num: $type) -> Self {
                MetricValue::Num(num as f64)
            }
        }
    };
}
num_type!(i32);
num_type!(i64);
num_type!(f64);

async fn statsd_listener(barrier: Arc<Barrier>, statsd_buf: Arc<RwLock<String>>) -> Result<String> {
    let socket = UdpSocket::bind("127.0.0.1:8125").await;
    let socket = match socket {
        Ok(s) => s,
        Err(error) => panic!("Cannot bind to 127.0.0.1:8125: {}", error),
    };
    barrier.wait().await; // indicates to main task that socket is listening

    loop {
        let mut buf = vec![0u8; 4096];
        let (recv, _peer) = socket.recv_from(&mut buf).await?;

        let datum = String::from_utf8(buf[..recv].into()).unwrap_or_else(|_| String::new());
        statsd_buf.write().await.push_str(&datum);
    }
}

fn get_statsd_metrics(metrics: &mut HashMap<String, MetricValue>, udp_data: String) -> Result<()> {
    let lines = udp_data.trim().lines();
    for line in lines {
        let metric: Vec<&str> = match line.split('|').next() {
            None => continue,
            Some(metric) => metric.split(':').collect(),
        };
        if metric.len() < 2 {
            continue;
        }
        metrics.insert(metric[0].into(), metric[1].parse::<f64>()?.into());
    }
    Ok(())
}

fn get_kernel_metrics(wall_time: f64, data: Rusage, metrics: &mut HashMap<String, MetricValue>) {
    metrics.insert("max.res.size".into(), data.max_res_size.into());
    metrics.insert("user.time".into(), data.user_time.into());
    metrics.insert("system.time".into(), data.system_time.into());

    let pct = (data.user_time + data.system_time) * 100.0 / wall_time;
    metrics.insert("cpu.pct.wall.time".into(), pct.into());
}

fn get_stdio() -> Stdio {
    match env::var("SIRUN_NO_STDIO") {
        Ok(_) => Stdio::null(),
        Err(_) => Stdio::inherit(),
    }
}

async fn run_setup(setup: &[String], env: &HashMap<String, String>) -> Result<()> {
    let mut code: i32 = 1;
    let mut attempts: u8 = 0;
    while code != 0 {
        if attempts == 100 {
            bail!("setup script did not complete successfully. aborting.");
        }
        let command = setup[0].clone();
        let args = setup.iter().skip(1);
        code = Command::new(command)
            .args(args)
            .envs(env.clone())
            .stdout(get_stdio())
            .stderr(get_stdio())
            .status()
            .await?
            .code()
            .expect("no exit code");
        if code != 0 {
            sleep(Duration::from_secs(1)).await;
            attempts += 1;
        }
    }

    Ok(())
}

async fn test_timeout(timeout: u64) {
    sleep(std::time::Duration::from_secs(timeout)).await;
    eprintln!("Timeout of {} seconds exceeded.", timeout);
    exit(1);
}

async fn run_test(
    config: &Config,
    mut metrics: &mut HashMap<String, MetricValue>,
    statsd_buf: Arc<RwLock<String>>,
) -> Result<()> {
    if let Some(timeout) = config.timeout {
        spawn(test_timeout(timeout));
    }

    let command = config.run[0].clone();
    let args = config.run.iter().skip(1);
    let start_time = std::time::Instant::now();
    let rusage_start = Rusage::new();
    let status = Command::new(command)
        .args(args)
        .envs(&config.env)
        .stdout(get_stdio())
        .stderr(get_stdio())
        .status()
        .await?;
    let duration = start_time.elapsed().as_micros();
    let rusage_result = Rusage::new() - rusage_start;
    metrics.insert("wall.time".to_owned(), (duration as f64).into());
    let status = status.code().expect("no exit code");
    if status != 0 && status <= 128 {
        eprintln!("Test exited with code {}, so aborting test.", status);
        exit(status);
    }
    get_kernel_metrics(duration as f64, rusage_result, &mut metrics);
    get_statsd_metrics(&mut metrics, statsd_buf.read().await.clone())?;
    statsd_buf.write().await.clear();
    Ok(())
}

async fn run_iteration(
    config: &Config,
    mut metrics: &mut HashMap<String, MetricValue>,
    statsd_buf: Arc<RwLock<String>>,
) -> Result<()> {
    let mut config: Config = config.clone();
    let json_config = serde_yaml::to_string(&config)?;
    config.env.insert("SIRUN_ITERATION".into(), json_config);
    config.cachegrind = false;
    let command = env::args().next().unwrap();
    let status = Command::new(command)
        .envs(&config.env)
        .stdout(get_stdio())
        .stderr(get_stdio())
        .status()
        .await?;
    let status = status.code().expect("no exit code");
    if status != 0 && status <= 128 {
        exit(status);
    }
    get_statsd_metrics(&mut metrics, statsd_buf.read().await.clone())?;
    statsd_buf.write().await.clear();
    Ok(())
}

#[async_std::main]
async fn main() -> Result<()> {
    let is_iteration = env::var("SIRUN_ITERATION").is_ok();
    let config = if is_iteration {
        serde_yaml::from_str(&env::var("SIRUN_ITERATION").unwrap()).unwrap()
    } else {
        let config_file = env::args().nth(1).expect("missing file argument");
        get_config(&config_file)?
    };
    if let Some(setup) = &config.setup {
        if env::var("SIRUN_SKIP_SETUP").is_err() {
            run_setup(&setup, &config.env).await?;
        }
    }

    let statsd_started = Arc::new(Barrier::new(2));
    let statsd_buf = Arc::new(RwLock::new(String::new()));
    if !is_iteration {
        spawn(statsd_listener(statsd_started.clone(), statsd_buf.clone()));
        statsd_started.wait().await; // waits for socket to be listening
    }

    let mut metrics: HashMap<String, MetricValue> = HashMap::new();
    if is_iteration || config.iterations == 1 {
        run_test(&config, &mut metrics, statsd_buf.clone()).await?;
    } else {
        let mut iterations = Vec::new();
        for _ in 0..config.iterations {
            let mut iteration_metrics = HashMap::new();
            run_iteration(&config, &mut iteration_metrics, statsd_buf.clone()).await?;
            iterations.push(iteration_metrics);
        }
        metrics.insert("iterations".into(), MetricValue::Arr(iterations));
    }

    if is_iteration {
        let buf = format!(
            "max.res.size:{}|g\nuser.time:{}|g\nsystem.time:{}|g\nwall.time:{}|g\ncpu.pct.wall.time:{}|g\n",
            metrics.remove("max.res.size").unwrap().as_f64(),
            metrics.remove("user.time").unwrap().as_f64(),
            metrics.remove("system.time").unwrap().as_f64(),
            metrics.remove("wall.time").unwrap().as_f64(),
            metrics.remove("cpu.pct.wall.time").unwrap().as_f64()
        );
        let sock = UdpSocket::bind("127.0.0.1:0").await?;
        sock.send_to(buf.as_bytes(), "127.0.0.1:8125").await?;
    } else {
        if config.cachegrind {
            let command = "valgrind";
            let mut args = vec![
                "--tool=cachegrind".to_owned(),
                "--trace-children=yes".to_owned(),
                // Set some reasonable L1 and LL values. It is important that these
                // values are consistent across runs, instead of the default.
                "--I1=32768,8,64".to_owned(),
                "--D1=32768,8,64".to_owned(),
                "--LL=8388608,16,64".to_owned(),
            ];
            args.append(&mut config.run.clone());
            let output = Command::new(command)
                .args(args)
                .envs(&config.env)
                .output()
                .await?;
            let stderr = String::from_utf8_lossy(&output.stderr);

            let lines = stderr.trim().lines().filter(|x| x.contains("I   refs:"));
            let mut instructions: f64 = 0.0;
            for line in lines {
                instructions += line
                    .trim()
                    .split_whitespace()
                    .last()
                    .expect("Bad cachegrind output: invalid instruction ref line")
                    .replace(",", "")
                    .parse::<f64>()
                    .expect("Bad cachegrind output: invalid number");
            }
            if instructions <= 0.0 {
                eprintln!("Bad cachegrind output: no instructions parsed");
                exit(1);
            }
            metrics.insert("instructions".into(), instructions.into());
        }

        if let Ok(hash) = env::var("GIT_COMMIT_HASH") {
            metrics.insert("version".into(), hash.into());
        }
        if let Some(name) = config.name {
            metrics.insert("name".into(), name.into());
        }
        if let Some(variant) = config.variant {
            metrics.insert("variant".into(), variant.into());
        }

        println!("{}", json!(metrics).to_string());
    }

    Ok(())
}
