extern crate dotenv;

use crate::events::CoreEvent;

use advisory_lock::{AdvisoryFileLock, FileLockMode};
use libcrio::Cli;
use log::{debug, error, info};
use serde_json::json;
use serde_json::Value;
use std::env;
use std::fs::{File, write, remove_dir_all, create_dir, create_dir_all};
use std::io;
use std::io::prelude::*;
use std::process;
use std::sync::mpsc::channel;
use std::thread;
use std::time::Duration;
use tar::Builder;
use flate2::Compression;
use flate2::write::GzEncoder;

mod config;
mod events;
mod logging;

fn main() -> Result<(), anyhow::Error> {
    let (send, recv) = channel();
    let cc = config::CoreConfig::new()?;
    let recv_time: u64 = cc.timeout as u64;
    thread::spawn(move || {
        let result = handle(cc);
        send.send(result).unwrap();
    });

    let result = recv.recv_timeout(Duration::from_secs(recv_time));

    match result {
        Ok(inner_result) => inner_result,
        Err(_error) => {
            error!("Timeout error during coredump processing.");
            process::exit(32);
        }
    }
}

fn handle(mut cc: config::CoreConfig) -> Result<(), anyhow::Error> {
    cc.set_namespace("default".to_string());
    let l_log_level = cc.log_level.clone();
    let log_path = logging::init_logger(l_log_level)?;
    debug!("Arguments: {:?}", env::args());

    info!(
        "Environment config:\n IGNORE_CRIO={}\nCRIO_IMAGE_CMD={}\nUSE_CRIO_CONF={}",
        cc.ignore_crio, cc.image_command, cc.use_crio_config
    );

    info!("Set logfile to: {:?}", &log_path);
    debug!("Creating dump for {}", cc.get_templated_name());

    let l_crictl_config_path = cc.crictl_config_path.clone();

    let config_path = if cc.use_crio_config {
        Some(
            l_crictl_config_path
                .into_os_string()
                .to_string_lossy()
                .to_string(),
        )
    } else {
        None
    };
    let l_bin_path = cc.bin_path.clone();
    let l_image_command = cc.image_command.clone();
    let cli = Cli {
        bin_path: l_bin_path,
        config_path,
        image_command: l_image_command,
    };
    let pod_object = cli.pod(&cc.params.hostname).unwrap_or_else(|e| {
        error!("{}", e);
        // We fall through here as the coredump and info can still be captured.
        json!({})
    });

    // match the label filter if there's one, and skip the whole process if it doesn't match
    if !cc.pod_selector_label.is_empty() {
        debug!(
            "Pod selector specified. Will record only if pod has label {}",
            &cc.pod_selector_label
        );
        let pod_labels = pod_object["labels"].as_object().unwrap();
        // check if pod_labels has pod_selector_label
        if pod_labels.get(&cc.pod_selector_label).is_none() {
            info!(
                "Skipping pod as it did not match selector label {}",
                &cc.pod_selector_label
            );
            process::exit(0);
        }
    } else {
        debug!("No pod selector specified, selecting all pods");
    }

    let namespace = pod_object["metadata"]["namespace"]
        .as_str()
        .unwrap_or("unknown");

    cc.set_namespace(namespace.to_string());

    let podname = pod_object["metadata"]["name"].as_str().unwrap_or("unknown");

    cc.set_podname(podname.to_string());

    // Create the base tar file that we are going to put everything into
    let file = match File::create(cc.get_tar_full_path()) {
        Ok(v) => v,
        Err(e) => {
            error!("Failed to create file: {}", e);
            process::exit(1);
        }
    };
    file.lock(FileLockMode::Exclusive)?;
    let mut tar_core = Builder::new(file);

    match create_dir_all("/tmp/core") {
        Ok(_) => println!("Folder is created successfully."),
        Err(e) => println!("Error while creating folder: {}", e),
    }

    debug!(
        "Create a JSON file to store the dump meta data\n{}",
        cc.get_dump_info_filename()
    );

    match write(format!("{}/{}","/tmp/core",cc.get_dump_info_filename()), cc.get_dump_info().as_bytes()) {
        Ok(v) => v,
        Err(e) => {
            error!("Error starting dump file in temp file \n{}", e);
            tar_core.finish()?;
            // file.unlock()?;
            remove_dir_all("/tmp/core").unwrap();
            process::exit(1);
        }
    };


    // Pipe the core file to zip
    let core_file = match File::create(format!("{}/{}.gz","/tmp/core",cc.get_core_filename())) {
        Ok(v) => v,
        Err(e) => {
            error!("Failed to create core file: {}", e);
            remove_dir_all("/tmp/core").unwrap();
            process::exit(1);
        }
    };
    core_file.lock(FileLockMode::Exclusive)?;
    let mut encoder = GzEncoder::new(&core_file, Compression::fast());

    let stdin = io::stdin();
    let mut stdin = stdin.lock();

    match io::copy(&mut stdin, &mut encoder) {
        Ok(v) => v,
        Err(e) => {
            error!("Error writing core file \n{}", e);
            core_file.unlock();
            remove_dir_all("/tmp/core").unwrap();
            process::exit(1);
        }
    };
    encoder.finish()?;
    core_file.unlock()?;


    if cc.ignore_crio {
        if cc.core_events {
            let tar_name = format!("{}.tar", cc.get_templated_name());
            let evtdir = format!("{}", cc.event_location.display());
            let evt = CoreEvent::new_no_crio(cc.params, tar_name);
            evt.write_event(&evtdir)?;
        }
        tar_core.append_dir_all("core","/tmp/core").unwrap();
        tar_core.finish()?;
        remove_dir_all("/tmp/core").unwrap();
        // file.unlock()?;
        process::exit(0);
    }

    debug!("Using runtime_file_name:{}", cc.get_pod_filename());

    match write(format!("{}/{}","/tmp/core",cc.get_pod_filename()), pod_object.to_string().as_bytes()) {
        Ok(v) => v,
        Err(e) => {
            error!("Error starting dump file in temp file \n{}", e);
            tar_core.finish()?;
            // file.unlock()?;
            process::exit(1);
        }
    };

    // TODO: Check logging of more than one pod retured
    let pod_id = match pod_object["id"].as_str() {
        Some(v) => v,
        None => {
            error!("Failed to get pod id");
            tar_core.finish()?;
            // file.unlock()?;
            remove_dir_all("/tmp/core").unwrap();
            process::exit(1);
        }
    };

    // With the pod_id get the runtime information from crictl
    debug!("Getting inspectp output using pod_id:{}", pod_id);

    let inspectp = cli.inspect_pod(pod_id).unwrap_or_else(|e| {
        error!("Failed to inspect pod {}", e);
        json!({})
    });
    debug!("Starting inspectp file\n{}", cc.get_inspect_pod_filename());

    match write(format!("{}/{}","/tmp/core",cc.get_inspect_pod_filename()), cc.get_inspect_pod_filename()) {
        Ok(v) => v,
        Err(e) => {
            error!("Error starting dump file in temp file \n{}", e);
            tar_core.finish()?;
            // file.unlock()?;
            remove_dir_all("/tmp/core").unwrap();
            process::exit(1);
        }
    };


    // Get the container_image_name based on the pod_id
    let ps_object = match cli.pod_containers(pod_id) {
        Ok(v) => v,
        Err(e) => {
            error!("{}", e);
            tar_core.finish()?;
            // file.unlock()?;
            remove_dir_all("/tmp/core").unwrap();
            process::exit(1);
        }
    };

    debug!("Starting ps file \n{}", cc.get_ps_filename());
    match write(format!("{}/{}","/tmp/core",cc.get_ps_filename()), ps_object.to_string().as_bytes()) {
        Ok(v) => v,
        Err(e) => {
            error!("Error starting dump file in temp file \n{}", e);
            tar_core.finish()?;
            // file.unlock()?;
            remove_dir_all("/tmp/core").unwrap();
            process::exit(1);
        }
    };

    // this still have bug, please do not use it
    debug!("Successfully got the process details {}", ps_object);
    let mut images: Vec<Value> = vec![];
    if let Some(containers) = ps_object["containers"].as_array() {
        for (counter, container) in containers.iter().enumerate() {
            let img_ref = match container["imageRef"].as_str() {
                Some(v) => v,
                None => {
                    error!("Failed to get containerid {}", "");
                    break;
                }
            };
            let log =
                cli.tail_logs(container["id"].as_str().unwrap_or_default(), cc.log_length).unwrap_or_else(|e| {
                    error!("Error finding logs:\n{}", e);
                    "".to_string()
                });
            debug!("Starting log file \n{}", cc.get_log_filename(counter));
            match write(format!("{}/{}","/tmp/core",cc.get_log_filename(counter)), log.to_string().as_bytes()) {
                Ok(v) => v,
                Err(e) => {
                    error!("Error starting dump file in temp file \n{}", e);
                    tar_core.finish()?;
                    // file.unlock()?;
                    remove_dir_all("/tmp/core").unwrap();
                    process::exit(1);
                }
            };
            debug!("found img_id {}", img_ref);
            let image = cli.image(img_ref).unwrap_or_else(|e| {
                error!("Error finding image:\n{}", e);
                json!({})
            });

            let img_clone = image.clone();
            images.push(img_clone);
            debug!("Starting image file \n{}", cc.get_image_filename(counter));
            match write(format!("{}/{}","/tmp/core",cc.get_image_filename(counter)), image.to_string().as_bytes()) {
                Ok(v) => v,
                Err(e) => {
                    error!("Error starting dump file in temp file \n{}", e);
                    tar_core.finish()?;
                    // file.unlock()?;
                    remove_dir_all("/tmp/core").unwrap();
                    process::exit(1);
                }
            };

            debug!(
                "Getting logs for container id {}",
                container["id"].as_str().unwrap_or_default()
            );
        }
    };

    tar_core.append_dir_all("core","/tmp/core").unwrap();
    tar_core.finish()?;
    match remove_dir_all("/tmp/core") {
        Ok(_) => println!("Folder is deleted successfully."),
        Err(e) => println!("Error while deleting folder: {}", e),
    }
    // file.unlock()?;
    if cc.core_events {
        let tar_name = format!("{}.tar", cc.get_templated_name());
        let evtdir = format!("{}", cc.event_location.display());
        let evt = CoreEvent::new(cc.params, tar_name, pod_object, images);
        evt.write_event(&evtdir)?;
    }
    Ok(())
}
