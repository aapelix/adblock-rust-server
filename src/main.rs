use std::env::var;
use std::fs;
use std::io::prelude::*;
use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use adblock::lists::FilterSet;
use adblock::lists::ParseOptions;
use adblock::request::Request;
use adblock::Engine;

use attohttpc;

enum InitType {
    Default,
    Reload,
    Update,
}

/// Handles communication with clients connecting to the server.
///
/// * `stream` - Data stream representing the connection between the client and the server.
/// * `blocker` - Pointer to the Engine struct which should be used for decision making.
fn handle_client(mut stream: UnixStream, mut blocker: Arc<Engine>) {
    let reader = BufReader::new(stream.try_clone().unwrap());
    for line in reader.lines() {
        let line = line.unwrap();
        let mut parts = line.split(' ');
        let mut res = String::new();

        match parts.next().unwrap() {
            "n" => {
                // network request
                let req_url = parts.next().unwrap();
                let source = parts.next().unwrap();
                let req_type = parts.next().unwrap();

                let req = Request::new(&req_url, &source, &req_type).unwrap();

                let result = blocker.check_network_request(&req);

                if result.matched == true {
                    res.push('1');
                } else {
                    res.push('0');
                }
            }
            "c" => {
                // cosmetic request
                let url = parts.next().unwrap();
                let resources = blocker.url_cosmetic_resources(url);
                let ids: Vec<String> = parts
                    .next()
                    .unwrap()
                    .split('\t')
                    .map(|x| x.to_string())
                    .collect();
                let classes: Vec<String> = parts
                    .next()
                    .unwrap()
                    .split('\t')
                    .map(|x| x.to_string())
                    .collect();

                let mut selectors =
                    blocker.hidden_class_id_selectors(&classes, &ids, &resources.exceptions);
                let mut hides: Vec<String> = resources.hide_selectors.into_iter().collect();
                selectors.append(&mut hides);
                let mut style = selectors.join(", ");

                if style.len() != 0 {
                    style.push_str(" { display: none !important; }");
                }
                style.push('\n');

                res.push_str(&style);
            }
            "r" => {
                // reload engine request
                blocker = Arc::new(setup_blocker(InitType::Reload));
                res.push('0');
            }
            "u" => {
                // force update request
                blocker = Arc::new(setup_blocker(InitType::Update));
                res.push('0');
            }
            _ => {
                res.push_str("Unknown code supplied");
            }
        };

        stream.write(res.as_bytes()).unwrap();
    }
}

/// Updates the specified filter list, returns the URL with the expiration timestamp.
///
/// The timestamp won't be appended if the filter list doesn't contain the `Expired` field.
///
/// * `url` - URL of the filter list that should be updated.
/// * `lists_dir` - Directory path where the filter list should be saved.
fn update_list(url: &str, lists_dir: &str) -> String {
    let filename = url.split('/').last().unwrap();
    let res = attohttpc::get(&url).send().unwrap();
    let mut f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(lists_dir.to_owned() + "/" + filename)
        .unwrap();

    res.write_to(&f).unwrap();

    f.seek(std::io::SeekFrom::Start(0)).unwrap();

    let reader = BufReader::new(f);
    for line in reader.lines() {
        let line = line.unwrap();
        if line.contains("! Expires: ") {
            let days = line.split(' ').nth(2).unwrap().parse::<u64>().unwrap() * 24 * 3600;
            let stamp = std::time::Duration::new(days, 0)
                + SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

            return url.to_string() + " " + &stamp.as_secs().to_string();
        }
    }

    return url.to_string();
}

/// Parses the URL file and returns a boolean stating if the configuration was updated.
///
/// Process every URL in the `urls_file`. If the file doesn't exist, it will be created.
///
/// * `urls_file` - Path of the URL file.
/// * `lists_dir` - Directory path where the filter lists should be saved.
/// * `force_update` - Boolean which when set to true will forcefully update every filter list
/// present in the URL file, even though it doesn't have to be updated (based on the 'Expires'
/// field)
fn parse_urls(urls_file: &str, lists_dir: &str, force_update: bool) -> bool {
    fs::create_dir_all(&lists_dir).unwrap();
    let mut updated = false;

    if Path::new(&urls_file).exists() {
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&urls_file)
            .unwrap();
        let reader = BufReader::new(file.try_clone().unwrap());
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap();
        let mut out = String::new();

        for line in reader.lines() {
            let line = line.unwrap();
            let mut parts = line.split(' ');
            let url = parts.next().unwrap();

            if !line.starts_with('#')
                && (force_update
                    || parts.clone().count() == 0
                    || parts.next().unwrap().parse::<u64>().unwrap() < timestamp.as_secs())
            {
                // list needs to be updated
                updated = true;
                out.push_str(&update_list(&url, &lists_dir));
            } else {
                out.push_str(&line);
            }

            out.push('\n');
        }

        file.seek(std::io::SeekFrom::Start(0)).unwrap();
        file.write_all(&out.into_bytes()).unwrap();
    } else {
        let mut file = fs::File::create(urls_file).unwrap();
        file.write(b"# Add your filter list urls here; lines starting with # will be ignored; timestamps right after urls determine the expiration time\n").unwrap();
    }

    return updated;
}

/// Initializes the blocking engine from the adblock-rust crate, returns the initialized engine.
///
/// If the engine configuration wasn't updated the engine data will be loaded from the serialized
/// engine file, if it exists. If not, the files located in `lists_dir` will be processed as filter
/// lists.
///
/// * `engine_file` - Path of the serialized engine file.
/// * `lists_dir` - Directory path where the filter lists should be saved.
/// * `updated` - Boolean which when set to true will cause the serialized engine file to be
/// ingored.
fn init_engine(engine_file: &str, lists_dir: &str, updated: bool) -> Engine {
    if Path::new(&engine_file).exists() && !updated {
        let mut blocker = Engine::new(true);
        let data = fs::read(engine_file);
        if data.is_ok() && blocker.deserialize(&data.unwrap()).is_ok() {
            return blocker;
        } else {
            return init_engine(engine_file, lists_dir, true);
        }
    } else {
        let mut rules = String::new();

        for entry in fs::read_dir(lists_dir).expect("Lists directory doesn't exist") {
            let path = entry.unwrap().path();

            if path.is_file() {
                let mut temp = String::new();
                let mut file = fs::File::open(path).unwrap();
                file.read_to_string(&mut temp).unwrap();
                rules.push_str(&temp);
            }
        }

        let mut filter_set = FilterSet::new(false);
        filter_set.add_filter_list(&rules, ParseOptions::default());
        let blocker = Engine::from_filter_set(filter_set, true);

        let data = blocker.serialize_raw();
        if data.is_ok() {
            let _ = fs::write(engine_file, data.unwrap());
        }

        return blocker;
    }
}

/// Starts handling incoming client connections.
///
/// The socket file will get automatically removed if it already exists.
///
/// * `socket_path` - Path of the socket file used for communication.
/// * `blocker` - The inialized Engine that should be used for filtering.
fn start_server(socket_path: &str, blocker: Engine) {
    if std::path::Path::new(socket_path).exists() {
        fs::remove_file(socket_path).expect("Can't remove Unix domain socket file");
    }

    let listener = UnixListener::bind(socket_path).expect("Can't bind to socket");
    println!("init-done");

    let blocker = Arc::new(blocker);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let blocker = blocker.clone();
                thread::spawn(move || handle_client(stream, blocker));
            }
            Err(err) => {
                eprintln!("Error: {}", err);
            }
        }
    }
}

/// Setup needed before initializing the server, returns the initialized Engine struct.
///
/// Consists of finding the $HOME directory, parsing the url file with `parse_urls` based on
/// specified `init_type` and creating the custom filters file if it doesn't exist.
///
/// * `init_type` - Specifies the type of Engine initialization.
fn setup_blocker(init_type: InitType) -> Engine {
    let home_dir = var("HOME").expect("Can't find environment variable $HOME");
    let config_dir = home_dir.to_owned() + "/.config/ars";
    let lists_dir = config_dir.to_owned() + "/lists";
    let engine_file = config_dir.to_owned() + "/engine";
    let urls_file = config_dir.to_owned() + "/urls";
    let custom_filters_file = lists_dir.to_owned() + "/custom";

    let updated = match init_type {
        InitType::Default => parse_urls(&urls_file, &lists_dir, false),
        InitType::Reload => {
            parse_urls(&urls_file, &lists_dir, false);
            true
        }
        InitType::Update => parse_urls(&urls_file, &lists_dir, true),
    };

    let custom_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&custom_filters_file);
    match custom_file {
        Ok(mut file) => {
            file.write(b"# Add your custom network and cosmetic filters here, lines starting with # will be ignored\n").unwrap();
        }
        Err(err) => {
            if err.kind() != std::io::ErrorKind::AlreadyExists {
                eprintln!("Can't create custom filters file: {}", err);
            }
        }
    }

    return init_engine(&engine_file, &lists_dir, updated);
}

/// The main function that initializes the blocker with `setup_blocker` and starts the server with
/// `start_server`.
fn main() {
    let blocker = setup_blocker(InitType::Default);
    start_server("/tmp/ars", blocker);
}
