use std::{panic, sync::{mpsc, Arc}, time::Duration, process::{Child}, fs::OpenOptions, io::{Write}, fmt};
use clap::{Parser, ValueHint};
use tokio::{sync::{oneshot, Semaphore}, time::Instant};
use walkdir::WalkDir;
use serde::{Serialize, Deserialize};
use std::path::PathBuf;
use ethers::etherscan::contract::{SourceCodeMetadata};
use std::process::{Command, Stdio};
use regex::Regex;
use lazy_static::lazy_static;

lazy_static! {
    static ref PANIC_REGEX: Regex = Regex::new(r"thread '.*?' panicked at (.+?)\n").unwrap();
    static ref ERROR_REGEX: Regex = Regex::new(r"(?s)Error:.*?31m([a-zA-Z0-9` .]{5,})").unwrap();
    static ref SUCCESS_REGEX: Regex = Regex::new(r"DONE ANALYZING IN: \d+ms\. Writing to cli\.\.\.\n$").unwrap();
}

const FIESTA_TOTAL_CONTRACTS: usize = 150_000;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {

    /// Path to the smart-contract-fiesta root directory
    #[clap(value_hint = ValueHint::FilePath, value_name = "PATH")]
    pub path: String,

    /// The number of contracts to run pyrometer on. Default is 5000
    /// If set to 0, all contracts will be analyzed
    #[clap(long, short)]
    pub num_contracts: Option<usize>,

    /// Timeout for each pyrometer process (secs). Default is 2 seconds, decimals supported.
    /// If set to 0, there will be no timeout. Not advised
    #[clap(long, short)]
    pub timeout: Option<f64>,

    /// Where to save the results file, default is "./data/results_MM-DD_HH-MM.csv"
    #[clap(long, short)]
    pub output: Option<String>,

    /// The number of concurrent proccesses to use for the analysis. Default is the number of cores
    #[clap(long, short)]
    pub jobs: Option<u8>,

    /// The number of contracts to initially skip over. Default is 0.
    /// This is intended for debugging purposes
    #[clap(long, short)]
    pub skip_contracts: Option<usize>,

}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum SourceType {
    /// source-string that always is read from main.sol
    SingleMain(String), 
    /// filename.sol and source-string tuples from multiple .sol files
    Multiple(Vec<(String, String)>),
    /// File contents string from contract.json
    EtherscanMetadata(SourceCodeMetadata),
}

impl fmt::Display for SourceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceType::SingleMain(_) => write!(f, "SingleFile"),
            SourceType::Multiple(_) => write!(f, "MultipleFiles"),
            SourceType::EtherscanMetadata(_) => write!(f, "JSON"),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FiestaMetadata {
    #[serde(rename = "ContractName")]
    contract_name: String,
    #[serde(rename = "CompilerVersion")]
    compiler_version: String,
    #[serde(rename = "Runs")]
    runs: i64,
    #[serde(rename = "OptimizationUsed")]
    optimization_used: bool,
    #[serde(rename = "BytecodeHash")]
    bytecode_hash: String,
    #[serde(skip_serializing, skip_deserializing)]
    abs_path_to_dir: String,
    #[serde(skip_serializing, skip_deserializing)]
    source_type: Option<SourceType>,
}

impl FiestaMetadata {
    pub fn compiler_is_supported(&self) -> bool {
        self.compiler_version.starts_with("v0.8.") && !self.compiler_version.contains("vyper")
    }

    pub fn update_path_to_dir(&mut self, path_to_dir: &PathBuf) {
        self.abs_path_to_dir = path_to_dir.to_str().unwrap().to_string();
    }

    pub fn update_source_type(&mut self, source_type: SourceType) {
        self.source_type = Some(source_type);
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    // convert path to PathBuf
    let abs_fiesta_path = std::path::PathBuf::from(args.path.clone());

    // check if path exists and is a directory
    if !abs_fiesta_path.exists() && !abs_fiesta_path.is_dir(){
        eprintln!("The path {} does not exist or is not a dir", args.path);
        std::process::exit(1);
    }

    // check if output path exists, otherwise use default.
    let output_path = match args.output {
        Some(path) => {
            // check if path exists, otherwise create needed parent directories
            let path = std::path::PathBuf::from(path);
            let path_parent = path.parent().unwrap();
            if !path_parent.exists() {
                std::fs::create_dir_all(&path_parent).unwrap();
            }
            
            path
        },
        None => {
            let mut path = std::path::PathBuf::from("./data");
            path.push(format!("results_{}.csv", chrono::Local::now().format("%m-%d_%H-%M")));
            let path_parent = path.parent().unwrap();
            if !path_parent.exists() {
                std::fs::create_dir_all(&path_parent).unwrap();
            }
            path
        }
    };

    // check if jobs is set, otherwise use number of cores
    let jobs = match args.jobs {
        Some(jobs) => jobs,
        None => num_cpus::get() as u8,
    };

    // check if timeout is set, otherwise use default
    let (pyrometer_timeout, rx_loop_timeout) = match args.timeout {
        Some(timeout) => {
            if timeout == 0.0 {
                (1_000_000.0, 1_000_000.0) // inf
            } else {
                (timeout, timeout + 1.0)
            }
        },
        None => (2.0, 2.0 + 1.0),
    };

    // check if num_contracts is set, otherwise use default
    let num_contracts = match args.num_contracts {
        Some(num_contracts) => {
            if num_contracts == 0 {
                std::usize::MAX
            } else {
                num_contracts
            }
        },
        None => 5000,
    };

    // check if skip_contracts is set, otherwise use default
    let skip_contracts = match args.skip_contracts {
        Some(skip_contracts) => skip_contracts,
        None => 0,
    };


    let mut fiesta_metadatas: Vec<FiestaMetadata> = Vec::with_capacity(FIESTA_TOTAL_CONTRACTS);
    
    /*
    walk the directory and collect all bytecode hashes
    path -> organized_contracts -> XX -> bytecodehash -> metadata.json
    metadata ex: {"ContractName":"Vyper_contract","CompilerVersion":"vyper:0.3.1","Runs":0,"OptimizationUsed":false,"BytecodeHash":"832117d7cd8eb3c6a7677a71fd59bd258faf57c4434f57151d51950060922abd"}

    find metadata.json files -> serde_json::from_str -> ContractMetadata
    filter by CompilerVersion > v0.8.0 and doesnt contain "vyper"
    */
    let mut contract_count = 0;
    let mut skipped_count = 0;
    for entry in WalkDir::new(abs_fiesta_path.join("organized_contracts")) {
        let entry = entry.unwrap();
        let path = entry.path();
        // check if path is metadata.json
        if path.is_file() && path.file_name().unwrap() == "metadata.json" {
            // read the file
            let file = std::fs::File::open(path).unwrap();
            let mut metadata: FiestaMetadata = serde_json::from_reader(file).unwrap();
            // filter by compiler version
            if !metadata.compiler_is_supported() {
                continue;
            }

            if skipped_count < skip_contracts {
                skipped_count += 1;
                continue;
            }
            // update the path to the directory (without the metadata.json file on the path)
            let mut path_to_dir = path.to_path_buf();
            path_to_dir.pop();
            metadata.update_path_to_dir(&path_to_dir);
            fiesta_metadatas.push(metadata);
            contract_count += 1;
            if contract_count % 1000 == 0 {
                println!("Total of {} contracts added to analysis queue", contract_count);
            }
            if contract_count == num_contracts {
                break;
            }
        }
    }


    fiesta_metadatas.iter_mut().for_each(|metadata| { collect_contract_sources(metadata); });
    fiesta_metadatas.retain(|metadata| metadata.source_type.is_some());

    println!("Beginning analysis of {} contracts", fiesta_metadatas.len());

    // Create a channel for threads to send their results
    let (tx, rx) = mpsc::channel();

    // Create a oneshot to signal the rx loop to stop
    let (stop_tx, stop_rx) = oneshot::channel::<()>();

    // Create a thread that runs the rx loop
    let rx_handle = tokio::spawn(async move {
        rx_loop(rx, stop_rx, output_path, rx_loop_timeout).await;
    });

    let tx_handle = tokio::spawn(async move {
        tx_loop(fiesta_metadatas, tx, stop_tx, jobs.into(), pyrometer_timeout).await;
    });

    let _ = tokio::join!(tx_handle, rx_handle);
}




pub fn analyze_with_pyrometer(metadata: &FiestaMetadata) -> Child {
    
    match metadata.clone().source_type.unwrap() {
        SourceType::SingleMain(_sol) => {
            let path_to_file = PathBuf::from(metadata.abs_path_to_dir.clone()).join(format!("main.sol"));
            // reformat path_to_file as a string
            let path_to_file = path_to_file.to_str().unwrap();

            let child = Command::new("pyrometer")
                .args(&[path_to_file, "--debug"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("Failed to spawn process");

            return child
        },
        SourceType::Multiple(multiple_files) => {
            let substr_to_find = format!("contract {} ", metadata.contract_name);
            for (name, sol_string) in multiple_files {
                if sol_string.contains(&substr_to_find) {
                    let path_to_file = PathBuf::from(metadata.abs_path_to_dir.clone()).join(&name);
                    let path_to_file = path_to_file.to_str().unwrap();

                    let child = Command::new("pyrometer")
                        .args(&[path_to_file, "--debug"])
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()
                        .expect("Failed to spawn process");

                    return child
                }
            }
            panic!("Could not find contract name {} in multiple_files", metadata.contract_name);
        },
        SourceType::EtherscanMetadata(_source_metadata) => {
            let path_to_file = PathBuf::from(metadata.abs_path_to_dir.clone()).join(format!("contract.json"));
            let path_to_file = path_to_file.to_str().unwrap();
            let child = Command::new("pyrometer")
                .args(&[path_to_file, "--debug"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("Failed to spawn process");

            return child
        },
    }
}


pub async fn tx_loop(fiesta_metadatas: Vec<FiestaMetadata>, tx_result: mpsc::Sender<ResultMessage>, tx_stop: oneshot::Sender<()>, max_concurrent_processes: usize, pyrometer_timeout: f64) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .build()
        .unwrap();

    // Semaphore for limiting the number of concurrent processes
    let semaphore = Arc::new(Semaphore::new(max_concurrent_processes));

    let pyrometer_timeout_duration = Duration::from_secs_f64(pyrometer_timeout);
    let mut join_handles = Vec::new();

    for metadata in fiesta_metadatas {
        let tx = tx_result.clone();
        let semaphore = semaphore.clone();
        let permit = semaphore.acquire_owned().await;

        let join_handle = runtime.spawn(async move {
            // Spawn the child process
            let mut child = analyze_with_pyrometer(&metadata);
            
            let start_time = Instant::now();
            // Poll the child process in a loop until timeout is reached
            loop {
                match child.try_wait() {
                    Ok(Some(_status)) => {
                        let result_message = ResultMessage {
                            metadata: metadata.clone(),
                            child: Some(child),
                            time: start_time.elapsed().as_secs_f64(),
                        };
                        let _ = tx.send(result_message);
                        break;
                    }
                    Ok(None) => {
                        // Check if timeout is reached
                        if start_time.elapsed() > pyrometer_timeout_duration {
                            let _ = child.kill();
                            let result_message = ResultMessage {
                                metadata: metadata.clone(),
                                child: None,
                                time: pyrometer_timeout,
                            };
                            let _ = tx.send(result_message);
                            break;
                        }
                        // async sleep for a short duration to avoid busy waiting. this wait is also our resolution for pyro completion
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                    Err(e) => {
                        println!("Error while polling child process: {:?}", e);
                        break;
                    }
                }
            }
            
            // Drop the semaphore permit
            drop(permit);
        });

        join_handles.push(join_handle);
    }

    // Wait for all processes to complete
    for handle in join_handles {
        let _ = handle.await;
    }

    // Informing that all tasks have been dispatched
    tx_stop.send(()).unwrap();
    
    // drop the runtime in a synchronous context
    std::thread::spawn(move || {
        runtime.shutdown_background();
    }).join().unwrap();
}


pub async fn rx_loop(rx_result: mpsc::Receiver<ResultMessage>, mut rx_stop: oneshot::Receiver<()>, output_path: PathBuf, rx_loop_timeout: f64) {

    let results_writer = ResultsWriter {
        output_path: output_path.clone()
    };
    results_writer.initiate_headers_for_results_csv();

    let rx_loop_timeout = Duration::from_secs_f64(rx_loop_timeout);
    let mut parse_count = 0;
    let mut total_parsable = 0;
    
    // keep looping over the rx_result channel until the rx_stop channel is closed
    loop {
        match rx_stop.try_recv() {
            Ok(_) => {
                println!("Stopping rx_loop");
                break;
            }
            Err(_) => {
                // Use timeout to wait for the next message with a 5 seconds timeout
                match rx_result.recv_timeout(rx_loop_timeout) {
                    Ok(result_message) if result_message.child.is_some() => {
                        // println!("Received some result message");
                        let exit_type = check_child_exit(result_message.child.unwrap());
                        assert!(!matches!(exit_type, ExitType::PerformanceTimeout), "PerformanceTimeout should not be possible here");
                        results_writer.append_to_results_file(&result_message.metadata, &exit_type, result_message.time);
                        match &exit_type {
                            ExitType::Success => {
                                parse_count += 1;
                            },
                            _ => {},
                        }
                        total_parsable += 1;
                    },
                    Ok(result_message) => {
                        // only here when child is None
                        // Timeout hit on process, count as failure
                        // println!("Received none result message");
                        results_writer.append_to_results_file(&result_message.metadata, &ExitType::PerformanceTimeout, result_message.time);
                        total_parsable += 1;
                    },
                    Err(e) => {
                        match e {
                            mpsc::RecvTimeoutError::Timeout => {
                                println!("Timeout hit, quitting rx_loop");
                                return;
                            },
                            _ => {
                                println!("Error receiving from rx_result: {:?}", e);
                            }
                        }
                    },
                }
                println!("{}/{}: {:.2}%, Parsable/Total Parsable", parse_count, total_parsable, parse_count as f64 / total_parsable as f64 * 100.0);
            }
        }
    }
}

pub struct ResultMessage {
    metadata: FiestaMetadata,
    child: Option<Child>,
    time: f64,
}

#[derive(Clone, Debug)]
/// Categorizes pyrometer runs into one of these variants based on the stdout string
pub enum ExitType {
    /// Successful parse
    Success,
    /// Timeout occurred while parsing
    PerformanceTimeout,
    /// (rel_path_to_file:line_number:col)
    Error(String),
    /// Type of panic (stack overflow, etc.)
    ThreadPanic(String),
    /// Failed to interpret the output of pyrometer. (stdout, stderr)
    NonInterpreted(String, String),
}

impl fmt::Display for ExitType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExitType::Success => write!(f, "Success"),
            ExitType::PerformanceTimeout => write!(f, "PerformanceTimeout"),
            ExitType::Error(s) => write!(f, "Error: {}", s.replace(',', ":")),
            ExitType::ThreadPanic(s) => write!(f, "ThreadPanic: {}", s.replace(',', ":")),
            ExitType::NonInterpreted(_stdout, _stderr) => write!(f, "NonInterpreted Error"),
        }
    }
}

pub struct ResultsWriter {
    pub output_path: PathBuf,
}

impl ResultsWriter {
    pub fn convert_fields_to_header() -> String {
        "bytecode_hash,result,time (sec),source_type\n".to_string()
    }

    pub fn initiate_headers_for_results_csv(&self) {
        println!("Initiating headers for results at: {:?}", &self.output_path);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&self.output_path)
            .unwrap();
        
        let header_string = Self::convert_fields_to_header();
        file.write_all(header_string.as_bytes()).unwrap();
    }

    pub fn append_to_results_file(&self, metadata: &FiestaMetadata, exit_type: &ExitType, time: f64) {
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.output_path)
            .unwrap();

        let bytecode_hash = metadata.bytecode_hash.clone();
        let source_type = metadata.source_type.clone().unwrap();

        let result_row = ResultsRow::from(exit_type.clone(), bytecode_hash, source_type, time);
        
        let row_string = result_row.convert_to_csv_string();
    
        file.write_all(row_string.as_bytes()).unwrap();
    }
}

pub struct ResultsRow {
    pub bytecode_hash: String,
    pub result: ExitType,
    pub time: f64,
    pub source_type: SourceType,
}

impl ResultsRow {
    
    pub fn from(result: ExitType, bytecode_hash: String, source_type: SourceType, time: f64) -> Self {

        Self {
            bytecode_hash,
            result: result.clone(),
            time,
            source_type,
        }
    }

    pub fn convert_to_csv_string(&self) -> String {
        format!("{},{},{:.3},{}\n", self.bytecode_hash, self.result, self.time, self.source_type)
    }
}

pub fn check_child_exit(child: Child) -> ExitType {
    // determine if the exit status has panics, errors, etc.
    if child.stdout.is_some() & child.stderr.is_some() {
        let stdout = child.stdout.unwrap();
        let mut stdout_reader = std::io::BufReader::new(stdout);
        let mut stdout_string = String::new();
        std::io::Read::read_to_string(&mut stdout_reader, &mut stdout_string).unwrap();
        let mut stderr = child.stderr.unwrap();
        let mut stderr_string = String::new();
        std::io::Read::read_to_string(&mut stderr, &mut stderr_string).unwrap();

        
        // convert stdout into one of the ExitType variants
        convert_pyrometer_output_to_exit_type(stdout_string, stderr_string)

    } else {
        dbg!(&child);
        panic!("Child stdout is None")
    }
}

pub fn convert_pyrometer_output_to_exit_type(stdout_string: String, stderr_string: String) -> ExitType {
    // Check if the output is from stderr and contains the phrase "thread 'main' panicked at"
    if let Some(captures) = PANIC_REGEX.captures(&stderr_string) {
        return ExitType::ThreadPanic(captures[1].to_string());
    }
    
    // Check if the output is from stdout and contains an error message
    if let Some(captures) = ERROR_REGEX.captures(&stdout_string) {
        let error_message = format!("{}", captures[1].trim());
        return ExitType::Error(error_message);
    }
    
    // Check if the output is from stdout and contains a success message
    if SUCCESS_REGEX.is_match(&stdout_string) {
        return ExitType::Success;
    }
    
    // If none of the above patterns are matched, return a NonInterpreted variant.
    ExitType::NonInterpreted(stdout_string, stderr_string)
}

pub fn collect_contract_sources(metadata: &mut FiestaMetadata) {

    /*
    There will either be a main.sol file, several .sol files of different names, or a contracts.json file
    - first look for contracts.json
    - then look for one .sol file named main.sol
    - then look for multiple .sol files
    - edgecase is a single main.vy file that has misconfigured metadata.json... there's about 10 of these, we can skip.
    */
    let path_to_dir = std::path::PathBuf::from(&metadata.abs_path_to_dir);
    let mut path_to_contract = std::path::PathBuf::new();
    for entry in WalkDir::new(&path_to_dir) {
        let entry = entry.unwrap();
        let path = entry.path();
        // println!("Looking for contracts.json: {}", &path.display());
        if path.is_file() && path.file_name().unwrap() == "contract.json" {
            path_to_contract = path.to_path_buf();
            let json_string = std::fs::read_to_string(path_to_contract.clone()).unwrap();
            // println!("{:#?}", &json_string);
            let contract_metadata: SourceCodeMetadata = serde_json::from_str(&json_string).unwrap();
            metadata.update_source_type(SourceType::EtherscanMetadata(contract_metadata));            
            break;
        }
    }
    // if contracts.json wasnt found, look for multiple .sol files
    if path_to_contract == std::path::PathBuf::new() {
        let mut sol_files = Vec::new();
        for entry in WalkDir::new(&path_to_dir) {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_file() && path.extension().unwrap() == "sol" {
                sol_files.push(path.to_path_buf());
            }
        }
        // if there is only one .sol file, use that

        if sol_files.len() == 1 {
            path_to_contract = sol_files[0].to_path_buf();
            metadata.update_source_type(SourceType::SingleMain(std::fs::read_to_string(path_to_contract.clone()).unwrap()));
        } else if sol_files.len() == 0 {
            println!("Found no .sol files: {}. this is likely a main.vy that should be a main.sol. needs changed", &path_to_dir.display())
            // could go to path_to_contract and rename main.vy to main.sol
        }
        else {
            // if there are multiple .sol files, look for main.sol
            let mut multiple_files = sol_files.into_iter().map(|path| {
                let name = path.file_name().unwrap().to_str().unwrap().to_string();
                let string = std::fs::read_to_string(path).unwrap();
                (name, string)
            }).collect::<Vec<(String, String)>>();
            multiple_files.sort_by(|a, b| a.0.cmp(&b.0));
            metadata.update_source_type(SourceType::Multiple(multiple_files));
        }
    }
}