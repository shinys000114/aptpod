// SPDX-License-Identifier: GPL-3.0-only

//! Tokens, the own password, and users (admin).

use clap::Subcommand;
use serde_json::{Value, json};

use crate::client::{CliError, Client};
use crate::{fmt_time, print_json};

#[derive(Subcommand)]
pub enum AccountCmd {
    /// API tokens of the logged-in user.
    #[command(subcommand)]
    Token(TokenCmd),
    /// Changes the own password (asks for the current one).
    Passwd {
        /// Read the current and the new password from two stdin lines.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Users (admin).
    #[command(subcommand)]
    User(UserCmd),
}

#[derive(Subcommand)]
pub enum TokenCmd {
    Ls,
    /// Creates a token and prints it (e.g. for CI); it is shown only once.
    Create {
        name: String,
    },
    Revoke {
        id: i64,
    },
}

#[derive(Subcommand)]
pub enum UserCmd {
    Ls,
    Add {
        name: String,
        /// reader, publisher or admin.
        #[arg(long, default_value = "publisher")]
        role: String,
        #[arg(long)]
        password_stdin: bool,
    },
    /// Changes a user's role.
    Role {
        name: String,
        role: String,
    },
    /// Sets a user's password.
    Passwd {
        name: String,
        #[arg(long)]
        password_stdin: bool,
    },
    Del {
        name: String,
    },
}

fn new_password(stdin: bool, prompt: &str) -> Result<String, CliError> {
    let read = |p: &str| {
        if stdin {
            aptpod_core::term::read_stdin_line()
        } else {
            aptpod_core::term::read_password(p)
        }
        .map_err(|e| CliError::other(format!("reading the password: {e}")))
    };
    let a = read(prompt)?;
    if !stdin && read("Again: ")? != a {
        return Err(CliError::usage("the passwords differ"));
    }
    Ok(a)
}

pub fn run(c: &Client, cmd: AccountCmd, json: bool) -> Result<(), CliError> {
    match cmd {
        AccountCmd::Token(TokenCmd::Ls) => {
            let list: Vec<Value> = c.get("/api/v1/tokens", &[])?;
            if json {
                print_json(&Value::Array(list));
            } else {
                for t in &list {
                    println!(
                        "{}\t{}\tcreated {}\tlast used {}",
                        t["id"],
                        t["name"].as_str().unwrap_or(""),
                        fmt_time(t["created"].as_i64()),
                        t["last_used"]
                            .as_i64()
                            .map_or("never".into(), |x| fmt_time(Some(x)))
                    );
                }
            }
        }
        AccountCmd::Token(TokenCmd::Create { name }) => {
            let r: Value = c.post("/api/v1/tokens", &json!({ "name": name }))?;
            println!("{}", r["token"].as_str().unwrap_or_default());
        }
        AccountCmd::Token(TokenCmd::Revoke { id }) => {
            c.delete(&format!("/api/v1/tokens/{id}"))?;
        }
        AccountCmd::Passwd { password_stdin } => {
            let current = if password_stdin {
                aptpod_core::term::read_stdin_line()
            } else {
                aptpod_core::term::read_password("Current password: ")
            }
            .map_err(|e| CliError::other(format!("reading the password: {e}")))?;
            let new = new_password(password_stdin, "New password: ")?;
            let _: Value = c.post(
                "/api/v1/password",
                &json!({ "current": current, "new": new }),
            )?;
            println!("password changed");
        }
        AccountCmd::User(cmd) => user(c, cmd, json)?,
    }
    Ok(())
}

fn user(c: &Client, cmd: UserCmd, json: bool) -> Result<(), CliError> {
    match cmd {
        UserCmd::Ls => {
            let list: Vec<Value> = c.get("/api/v1/users", &[])?;
            if json {
                print_json(&Value::Array(list));
            } else {
                for u in &list {
                    println!(
                        "{}\t{}",
                        u["name"].as_str().unwrap_or(""),
                        u["role"].as_str().unwrap_or("")
                    );
                }
            }
        }
        UserCmd::Add {
            name,
            role,
            password_stdin,
        } => {
            let password = new_password(password_stdin, &format!("Password for {name}: "))?;
            let _: Value = c.post(
                "/api/v1/users",
                &json!({ "name": name, "password": password, "role": role }),
            )?;
            println!("added {name} ({role})");
        }
        UserCmd::Role { name, role } => {
            let _: Value = c.post(&format!("/api/v1/users/{name}"), &json!({ "role": role }))?;
        }
        UserCmd::Passwd {
            name,
            password_stdin,
        } => {
            let password = new_password(password_stdin, &format!("Password for {name}: "))?;
            let _: Value = c.post(
                &format!("/api/v1/users/{name}"),
                &json!({ "password": password }),
            )?;
        }
        UserCmd::Del { name } => c.delete(&format!("/api/v1/users/{name}"))?,
    }
    Ok(())
}
