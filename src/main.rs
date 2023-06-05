use std::{panic, sync::mpsc, thread};
use clap::{Parser, ValueHint};
use walkdir::WalkDir;
use serde::{Serialize, Deserialize};
use std::path::PathBuf;
use std::panic::AssertUnwindSafe;
use rayon::prelude::*;

use pyrometer::Analyzer;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {

    /// Either a path to the smart-contract-fiesta root directory
    #[clap(value_hint = ValueHint::FilePath, value_name = "PATH")]
    pub path: String,

    /// Where to save the diagnostics file, default is stdout
    #[clap(long, short)]
    pub output: Option<String>,

    /// The number of threads to use for the analysis. Default is the number of cores
    #[clap(long, short, default_value = "1")]
    pub jobs: u8,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum SourceType {
    SingleMain(String), // read string from main.sol
    Multiple(Vec<(String, String)>), // name and string tuples from multiple .sol files
    ContractsJson(String), // read string from contracts.json
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

fn main() {
    let args = Args::parse();
    // convert path to PathBuf
    let abs_fiesta_path = std::path::PathBuf::from(args.path.clone());

    // check if path exists and is a directory
    if !abs_fiesta_path.exists() && !abs_fiesta_path.is_dir(){
        eprintln!("The path {} does not exist or is not a dir", args.path);
        std::process::exit(1);
    }

    let mut fiesta_metadatas: Vec<FiestaMetadata> = Vec::with_capacity(150_000); // 149386 contracts
    
    /*
    walk the directory and collect all bytecode hashes
    path -> organized_contracts -> XX -> bytecodehash -> metadata.json
    {"ContractName":"Vyper_contract","CompilerVersion":"vyper:0.3.1","Runs":0,"OptimizationUsed":false,"BytecodeHash":"832117d7cd8eb3c6a7677a71fd59bd258faf57c4434f57151d51950060922abd"}

    find metadata.json files -> serde_json::from_str -> ContractMetadata
    filter by CompilerVersion > v0.8.0 and doesnt contain "vyper"
    */
    // let mut unsupported_count = 0;
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
            // update the path to the directory (without the metadata.json file on the path)
            let mut path_to_dir = path.to_path_buf();
            path_to_dir.pop();
            metadata.update_path_to_dir(&path_to_dir);
            fiesta_metadatas.push(metadata);
        }
    }


    fiesta_metadatas.iter_mut().for_each(|metadata| { collect_contract_sources(metadata); });
    fiesta_metadatas.retain(|metadata| metadata.source_type.is_some());

    println!("Beginning analysis of {} contracts", fiesta_metadatas.len());


    // let parse_results: Vec<_> = fiesta_metadatas.par_iter().map(|metadata| {
    //     let metadata = metadata.clone();

    
    //     // wrap in catch_unwind
    //     let result = panic::catch_unwind(AssertUnwindSafe(|| {
    //         analyze_with_pyrometer(metadata.clone())
    //     }));
    
    //     match result {
    //         Ok(result) => result, // if no panic occurred, return the result
    //         Err(_) => None,       // if a panic occurred, return None
    //     }
    // }).collect();

    // let mut parse_count = 0;
    // let mut total_parsable = 0;
    // for result in parse_results {
    //     match result {
    //         Some(true) => {
    //             // println!("good");
    //             parse_count += 1;
    //             total_parsable += 1;
    //         }
    //         Some(false) => {
    //             // println!("bad");
    //             total_parsable += 1;
    //         }
    //         None => {
    //             // println!("None");
    //         }
    //     }
    // }
    // println!("Parsed {} out of {} contracts", parse_count, total_parsable);



    // Create a channel for threads to send their results
    let (tx, rx) = mpsc::channel();


    // now that each metadata has a source type, we can analyze using pyrometer
    for metadata in fiesta_metadatas {
        // Clone the transmitter for each thread
        let tx = tx.clone();
        let metadata = metadata.clone();

        // Create a new thread with a large stack size (e.g., 8 MB)
        thread::Builder::new().name(metadata.bytecode_hash.clone()).stack_size(8 * 1024 * 1024).spawn(move || {
            // Process the file here
            let result = analyze_with_pyrometer(metadata);

            // Send the result back to the main thread
            tx.send(result).expect("Failed to send result");
        }).expect("Failed to spawn thread");
    }
    drop(tx);

    let mut parse_count = 0;
    let mut total_parsable = 0;
    // Collect results from the threads as they finish
    for result in rx {
        match result {
            Some(true) => {
                println!("good");
                parse_count += 1;
                total_parsable += 1;
            }
            Some(false) => {
                println!("bad");
                total_parsable += 1;
            }
            None => {
                println!("None");
            }
        }
    }
    println!("Parsed {} out of {} contracts", parse_count, total_parsable);
}

pub fn analyze_with_pyrometer(metadata: FiestaMetadata) -> Option<bool> {

    match metadata.source_type.unwrap() {
        SourceType::SingleMain(sol) => {
            // return None;
            // println!("Analyzing: {}", &metadata.abs_path_to_dir);
            let mut analyzer = Analyzer {
                root: PathBuf::from(metadata.abs_path_to_dir.clone()),
                ..Default::default()
            };

            // catch panics, and if so return false
            let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
                analyzer.parse(&sol, &PathBuf::from(metadata.abs_path_to_dir.clone()), true)
            }));

            match result {
                Ok((_maybe_entry, _all_sources)) => {
                    // Handle successful parsing here. If you want to return true
                    return Some(true);
                }
                Err(_) => {
                    // This will be executed if the parsing code panics.
                    return Some(false);
                }
            }
        },
        SourceType::Multiple(multiple_files) => {
            // println!("Found multiple .sol files: {}", &metadata.abs_path_to_dir);
            // (name, sol_string)
            // check whether the metadata.contract_name is in the sol_string
            let substr_to_find = format!("contract {} ", metadata.contract_name);
            for (name, sol_string) in multiple_files {
                if sol_string.contains(&substr_to_find) {
                    // println!("Analyzing: {}", &metadata.abs_path_to_dir);
                    let mut analyzer = Analyzer {
                        root: PathBuf::from(metadata.abs_path_to_dir.clone()),
                        ..Default::default()
                    };

                    let path_to_sol_file = PathBuf::from(metadata.abs_path_to_dir.clone()).join(name);
                    // println!("path_to_sol_file: {}", path_to_sol_file.display());
                    // catch panics, and if so return false
                    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
                        analyzer.parse(&sol_string, &path_to_sol_file, true)
                    }));

                    match result {
                        Ok((_maybe_entry, _all_sources)) => {
                            // Handle successful parsing here. If you want to return true
                            return Some(true);
                        }
                        Err(_) => {
                            // This will be executed if the parsing code panics.
                            return Some(false);
                        }
                    }
                }
            }
            panic!("Could not find contract name {} in multiple_files", metadata.contract_name);
        },
        SourceType::ContractsJson(_contracts_json) => {
            // hopefully can use compile_bb to handle this
        },
    }

    return None;
}


pub fn collect_contract_sources(metadata: &mut FiestaMetadata) {

    /*
    There will either be a main.sol file, several .sol files of different names, or a contracts.json file
    - first look for contract.json
    - then look for multiple .sol files
    - then look for main.sol
    */
    let path_to_dir = std::path::PathBuf::from(&metadata.abs_path_to_dir);
    let mut path_to_contract = std::path::PathBuf::new();
    for entry in WalkDir::new(&path_to_dir) {
        let entry = entry.unwrap();
        let path = entry.path();
        // println!("Looking for contracts.json: {}", &path.display());
        if path.is_file() && path.file_name().unwrap() == "contract.json" {
            path_to_contract = path.to_path_buf();
            metadata.update_source_type(SourceType::ContractsJson(std::fs::read_to_string(path_to_contract.clone()).unwrap()));            
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
        }
        else {
            // if there are multiple .sol files, look for main.sol
            // println!("Found multiple .sol files: {}", &path_to_dir.display());
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
