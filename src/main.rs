use nng::{Protocol, Socket, Error};
use std::process::Command;
use rfd::MessageDialog;
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
struct Settings {
    cris_tools_path: PathBuf,
    port: toml::Value,
}

fn load_settings() -> Result<Settings, toml::de::Error> {
    // Read the entire contents of the file
    let contents = fs::read_to_string("cris_tools_proxy.toml")
        .expect("Failed to read settings file");

    // Parse the file contents and deserialize into the Settings struct
    toml::from_str(&contents)
}

fn main() {
    let settings = match load_settings() {
        Ok(settings) => settings,
        Err(e) => {
            eprintln!("Failed to load settings: {:?}", e);
            std::process::exit(1);
        }
    };

    // Get the command line argument
    let arg = std::env::args().nth(1).expect("Missing argument");

    // Create a socket of type REQ (request)
    let socket = Socket::new(Protocol::Req0).expect("Failed to create socket");

    // Connect to the NNG server
    match socket.dial(format!("tcp://localhost:{}", settings.port).as_str()) {
        Ok(_) => {
            println!("Connected to server");
        }
        Err(e) => {
            println!("Failed to connect to server: {:?}", e);
            let choice = MessageDialog::new()
                .set_title("CRIS Tools not running")
                .set_description("For advanced voice commands to function CRIS Tools needs to be running.\nDo you want to launch it?")
                .set_buttons(rfd::MessageButtons::YesNo)
                .show();

            match choice {
                rfd::MessageDialogResult::Yes => {
                    println!("User chose Yes");
                    let output = Command::new(settings.cris_tools_path)
                        .spawn();

                    match output {
                        Ok(_) => {
                            println!("CRIS Tools launched successfully");
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            // Handle the NotFound error here
                            println!("CRIS Tools not found");
                            eprintln!("Failed to launch CRIS Tools: {:?}", e);
                        }
                        Err(e) => {
                            eprintln!("Failed to launch CRIS Tools: {:?}", e);
                        }
                    }
                },
                rfd::MessageDialogResult::No => println!("User chose No"),
                _ => println!("User closed or cancelled the dialog box"),
            }

            std::process::exit(1);
        }
    }

    // Send the argument to the server
    let message = "run/".to_string() + &arg;
    socket.send(message.as_bytes()).expect("Failed to send message");

    // Receive the response from the server
    match socket.recv() {
        Ok(response) => {
            let response = String::from_utf8(response.to_vec()).expect("Failed to receive response");
            println!("Response: {}", response);
        }
        Err(Error::TimedOut) => {
            println!("Failed to receive response: Timeout");
        }
        Err(e) => {
            println!("Failed to receive response: {:?}", e);
        }
    }
}