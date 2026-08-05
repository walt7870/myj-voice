use std::io::{self, Read};

use anyhow::{bail, Result};
use mjy_voice_shop_rs::admin_auth::{generate_admin_password, hash_password};

fn main() -> Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let command = arguments.next();
    if arguments.next().is_some() {
        bail!("usage: mjy-admin-password <hash|generate>");
    }

    match command.as_deref() {
        Some(command) if command == std::ffi::OsStr::new("hash") => {
            let mut password = String::new();
            io::stdin().read_to_string(&mut password)?;
            let password = password.trim_end_matches(['\r', '\n']);
            println!("{}", hash_password(password)?);
            Ok(())
        }
        Some(command) if command == std::ffi::OsStr::new("generate") => {
            println!("{}", generate_admin_password());
            Ok(())
        }
        _ => bail!("usage: mjy-admin-password <hash|generate>"),
    }
}
