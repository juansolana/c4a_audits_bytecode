use dotenv::dotenv;
use std::env;
use reqwest::Error;
use reqwest::header;
use serde_json::Value;
use scraper::{Html, Selector};
use regex::Regex;
use std::future::Future;
use async_recursion::async_recursion;
use std::collections::HashMap;
use std::path::Path;
use tokio::process::Command; 
use tokio::fs::remove_dir_all;
use std::io::{Error as IoError, ErrorKind};
use tokio::time::{sleep, Duration};
use git2::Repository;
use headless_chrome::{Browser, protocol::cdp::Page::CaptureScreenshotFormatOption, Element};
use headless_chrome::protocol::cdp::Page;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Links {
    #[serde(rename = "self")]
    pub self_link: Option<String>,
    pub git: Option<String>,
    pub html: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RepoContent {
    pub name: Option<String>,
    pub path: Option<String>,
    pub sha: Option<String>,
    pub size: Option<u64>,
    pub url: Option<String>,
    #[serde(rename = "type")]
    pub _type: Option<String>,
    #[serde(rename = "html_url")]
    pub html_url: Option<String>,
    #[serde(rename = "git_url")]
    pub git_url: Option<String>,
    #[serde(rename = "download_url")]
    pub download_url: Option<String>,
    #[serde(rename = "type")]
    pub content_type: Option<String>,
    #[serde(rename = "_links")]
    pub links: Option<Links>,
}

type ResponseContent = Option<Vec<RepoContent>>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {   
    // dotenv().ok();

    let mut active_contests = get_contests("active").await?;
    let mut upcoming_contests = get_contests("upcoming").await?;
    active_contests.append(&mut upcoming_contests);

    println!("Contests: {:#?}", &active_contests);

    match process(&active_contests).await {
        Ok(all_results) => {
            println!("Done: {}", all_results.len());
        },
        Err(e) => eprintln!("An error occurred: {}", e),
    }
    Ok(())
}

/// Returns the href attribute of the html element passed.
fn get_attr(elt: &headless_chrome::Element, attr: &str) -> String {
    match elt.call_js_fn(&format!("function() {{ return this.getAttribute(\"{}\"); }}", attr), vec![], true).unwrap().value {
        Some(Value::String(s)) => s,
        _ => panic!("Expected string"),
    }
}

/// Returns a vector of contests' repos that have div with class `contest_status` along with "contest-tile".
/// It uses headless chromes to make a browser instance, navigate to the contests page and get the
/// repos of the matching contests.
async fn get_contests(contest_status: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut contests = Vec::new();
    let browser = Browser::default()?;
    let tab = browser.new_tab()?;
    
    // Navigate to the Code4rena contests page and wait for it to load
    tab.navigate_to("https://code4rena.com/contests")?;
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    tab.wait_for_element("div.contests-page")?;
    
    // Find all the contest divs that are have a class with contest_status value
    let find_class_status = format!("div.contest-tile.{}",contest_status);
    let contest_elements = match tab.find_elements(&find_class_status) {
        Ok(elements) => elements,
        Err(_) => return Ok(contests),
    };
    
    // Iterate over each contest element and extract the href attribute for the github repo
    for contest_element in contest_elements {      
        let footer = match contest_element.find_elements("a.dropdown__button") {
            Ok(foot) => {
                for f in foot {
                    let href = get_attr(&f, "href");
                    if href.starts_with("https://github.com/") {
                        contests.push(href);
                    }
                }
            },
            Err(_) => continue,  // If we can't find the name element, skip this contest
        };        
    }
    tab.close(true)?; 
    Ok(contests)
}

/// Returns a vector containing the contest's repo name and a vector that contains the contracts in the repo along with their bytecode.
async fn process(target_repos: &Vec<String>) -> Result<Vec<(String, Vec<(String, String)>)>, Box<dyn std::error::Error>> {
    // let github_token = env::var("GITHUB_TOKEN").expect("GITHUB_TOKEN must be set");

    let mut all_results: Vec<(String, Vec<(String, String)>)> = Vec::new();

    let repo_client = reqwest::Client::new();
    for contest in target_repos {

        let parts: Vec<&str> = contest.split('/').collect();
        let (owner, repo) = (parts[3], parts[4]);
        let repo_fetch_url = format!("https://github.com/{}/{}", &owner, &repo);
 
        let contents_url = format!("https://api.github.com/repos/{}/{}/contents", owner, repo);
        println!("\nContest Repo: {:#?}", contents_url);
        
        let response = repo_client.get(&contents_url)
            // .header("Authorization", format!("token {}", github_token))
            .header(header::USER_AGENT, "Rust")
            .send()
            .await?;                            

        if response.status().as_u16() > 400 {
            println!("Repo not accessible. {:#?}", response.status().as_u16());
            continue;
        }
        
        // If repo already exists, delete it to clone latest
        let repo_path_str = format!("./repos/{}", &repo);
        let repo_path = Path::new(&repo_path_str);
        if repo_path.exists() {
            remove_dir_all(&repo_path_str);
            let result = remove_dir_all(&repo_path_str).await;
            if result.is_err() {
                eprintln!("Failed to delete repository at {}: {}", repo_path_str, result.unwrap_err());
            }
        }
        // Clone repo locally and compile contracts
        let repo_fetch = match Repository::clone(&repo_fetch_url, &repo_path_str) {
            Ok(repo_fetch) => {
                repo_fetch;
                println!("Repo cloned. Attempting compilation.");

                let output = Command::new("forge")
                    .current_dir(&repo_path_str)
                    .arg("compile")
                    .output()
                    .await?;  // executes the command                                    

                if !output.status.success() {
                    let err_msg = format!("forge build failed with status {}: {}", output.status, String::from_utf8_lossy(&output.stderr));
                    // return Err(Box::new(IoError::new(ErrorKind::Other, err_msg)));
                }
            },
            Err(e) => panic!("failed to clone: {}", e),
        };

        let body: String = response.text().await?;
        
        let contents: Vec<RepoContent> = serde_json::from_str(&body).unwrap();
        let repo_results = process_contents(&contents, &repo_client, &owner, &repo).await?;
        // println!("REPO RESULT: {:#?}", repo_results);
        if !repo_results.is_empty() {
            // If we found any contracts in this repo, add it to the overall results.
            all_results.push((repo.to_string(), repo_results));
        }
        println!("ALL RESULT: {:#?}", all_results);

        // Delete the cloned repo
        let result = remove_dir_all(&repo_path_str).await;
        if result.is_err() {
            eprintln!("Failed to delete repository at {}: {}", repo_path_str, result.unwrap_err());
        }
    }
    Ok(all_results)
}

#[async_recursion]
async fn process_contents(contents: &Vec<RepoContent>, repo_client: &reqwest::Client, owner: &str, repo: &str) -> Result<Vec<(String, String)>, Box<dyn std::error::Error>> {
    // let github_token = env::var("GITHUB_TOKEN").expect("GITHUB_TOKEN must be set");

    let mut repo_results: Vec<(String, String)> = Vec::new();
    for content in contents {
        match content._type.as_deref() {
            Some("file") => {
                if let Some(name) = &content.name {
                    if name.ends_with(".sol") && !name.ends_with(".t.sol") && !name.ends_with(".s.sol") && !name.contains("Test") {
                        if let Some(filename) = content.name.as_ref() {
                            match get_bytecode(&filename, &repo).await {
                                Ok(bytecode) => {
                                    println!("Bytecode exists for: {}", filename);
                                    repo_results.push((filename.clone(), bytecode));
                                }
                                Err(e) => {
                                    eprintln!("Error - Bytecode not found for: {}", filename);
                                    continue;
                                }
                            }
                        } else {
                            continue;
                        }
                    }
                }
            },
            Some("dir") => {
                // this is a directory, need to fetch its contents and process them
                if let Some(path) = &content.path {
                    let dir_url = format!("https://api.github.com/repos/{}/{}/contents/{}", owner, repo, path);
                    let response = repo_client.get(&dir_url)
                        // .header(header::AUTHORIZATION, format!("token {}", github_token))
                        .header("User-Agent", "Rust")
                        .send()
                        .await?;
                    let dir_contents: Vec<RepoContent> = response.json().await?;
                    let mut dir_results = process_contents(&dir_contents, repo_client, owner, repo).await?;
                    repo_results.append(&mut dir_results);
                }
            },
            _ => {
                continue;
            },
        }
    }
    Ok(repo_results)
}

/// Returns the pragma solidity line of the given file.
fn get_pragma_version(source: &str) -> Option<String> {
    let re = Regex::new(r"^pragma solidity (\^?[0-9.]+);").unwrap();
    for line in source.lines() {
        if let Some(cap) = re.captures(line) {
            return Some(cap.get(1)?.as_str().to_string());
        }
    }
    None
}

/// Returns the bytecode for a given contract in a given repo.
async fn get_bytecode(original_file_name: &str, repo: &str) -> Result<String, Box<dyn std::error::Error>> {
    let file_name = original_file_name.strip_suffix(".sol").unwrap();
    let repo_path = format!("./repos/{}", &repo);
    
    let output = Command::new("forge")
        .current_dir(&repo_path)
        .arg("inspect")
        .arg(&file_name)
        .arg("bytecode")
        .output()
        .await?;

    if !output.status.success() {
        let err_msg = format!("forge failed with status {}: {}", output.status, String::from_utf8_lossy(&output.stderr));
        return Err(Box::new(IoError::new(ErrorKind::Other, err_msg)));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}