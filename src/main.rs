use std::io::prelude::*;
use std::thread;
use std::os::unix::net::{UnixStream, UnixListener};
use std::io::BufReader;
use std::sync::Arc;
use std::fs;
use std::env::var;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use adblock::engine::Engine;
use adblock::lists::{FilterFormat, FilterSet};

use attohttpc;

fn handle_client(mut stream: UnixStream, blocker: Arc<Engine>) {
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

                let result = blocker.check_network_urls(req_url, source, req_type);

                if result.matched == true {
                    res.push('1');
                } else {
                    res.push('0');
                }
            },
            "c" => {
                // cosmetic request
                let url = parts.next().unwrap();
                let resources = blocker.url_cosmetic_resources(url);
                let ids : Vec<String> = parts.next().unwrap().split('\t').map(|x| x.to_string()).collect();
                let classes : Vec<String> = parts.next().unwrap().split('\t').map(|x| x.to_string()).collect();

                let mut selectors = blocker.hidden_class_id_selectors(&classes, &ids, &resources.exceptions);
                let mut hides : Vec<String> = resources.hide_selectors.into_iter().collect();
                selectors.append(&mut hides);
                let mut style = selectors.join(", ");

                if style.len() != 0 {
                    style.push_str(" { display: none !important; }");
                }
                style.push('\n');

                res.push_str(&style);
            },
            _ => {
                res.push_str("Unknown code supplied");
            }
        };

        stream.write(res.as_bytes()).unwrap();
    }
}

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
            let stamp = std::time::Duration::new(days, 0) + SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

            return url.to_string() + " " + &stamp.as_secs().to_string();
        }
    }
    
    return url.to_string();
}

fn parse_urls(urls_file: &str, lists_dir: &str) -> bool {
    fs::create_dir_all(&lists_dir).unwrap();
    let mut updated = false;

    if Path::new(&urls_file).exists() {
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&urls_file)
            .unwrap();
        let reader = BufReader::new(file.try_clone().unwrap());
        let timestamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap();
        let mut out = String::new();

        for line in reader.lines() {
            let line = line.unwrap();
            let mut parts = line.split(' ');
            let url = parts.next().unwrap();

            if parts.clone().count() == 0 || parts.next().unwrap().parse::<u64>().unwrap() < timestamp.as_secs() {
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
    }

    return updated;
}

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
        filter_set.add_filter_list(&rules, FilterFormat::Standard);
        let blocker = Engine::from_filter_set(filter_set, true);

        let data = blocker.serialize();
        if data.is_ok() {
            let _ = fs::write(engine_file, data.unwrap());
        }

        return blocker;
    }
}

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
                println!("Error: {}", err);
            }
        }
    }
}

fn main() {
    let home_dir = var("HOME").expect("Can't find environment variable $HOME");
    let config_dir = home_dir.to_owned() + "/.config/ars";
    let lists_dir = config_dir.to_owned() + "/lists";
    let engine_file = config_dir.to_owned() + "/engine";
    let urls_file = config_dir.to_owned() + "/urls";

    let updated = parse_urls(&urls_file, &lists_dir);
    let blocker = init_engine(&engine_file, &lists_dir, updated);
    start_server("/tmp/ars", blocker);
}
