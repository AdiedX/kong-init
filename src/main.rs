#![allow(dead_code)]

extern crate clap;
#[macro_use]
extern crate log;
extern crate pretty_env_logger;
extern crate regex;
extern crate semver;
extern crate serde;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate serde_json;
extern crate serde_yaml;

use clap::{App, Arg};
use client::KongApiClient;
use entity::{ApiInfo,
             ConfFileStyle,
             KongConf,
             LegacyKongConf,
             LegacyPluginInfo,
             LegacyPluginAppliedType,
             PluginInfo,
             RouteInfo,
             PluginTarget,
             ServiceInfo};
use regex::Regex;
use semver::Version;
use serde_yaml::Error;
use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::prelude::*;
use std::iter::FromIterator;
use std::thread::sleep;
use std::time::Duration;

mod client;
mod entity;

fn main() {
    pretty_env_logger::init();
    let matches = App::new("kong-init")
        .version("0.6.0")
        .about("")
        .arg(Arg::with_name("path")
            .required(true)
            .short("p")
            .long("path")
            .takes_value(true)
            .help("path to route defination file"))
        .arg(Arg::with_name("admin-url")
            .long("url")
            .takes_value(true)
            .help("admin url of kong-server"))
        .arg(Arg::with_name("")
            .long("wait")
            .short("w")
            .help("wait until kong-server is ready(suit for init under cloud environment)"))
        .get_matches();

    let tmpl_path = matches.value_of("path").unwrap();
    let admin_url = matches.value_of("admin-url").unwrap();
    info!("Start serving KongInit...");
    info!("Connecting to Kong on {} using {}", admin_url, tmpl_path);

    if let Err(e) = runc(tmpl_path, admin_url, false) {
//        error!("unable to init kong: {}", e);
        std::process::exit(1)
    }
}

struct ExecutionContext<'a> {
    kong_cli: Box<KongApiClient<'a>>,
    support_api: bool,
    support_service_route: bool,
    // legacy mode
    api_names: Vec<String>,
    // suggested mode
    service_name_id_mapping: HashMap<String, String>,
    route_name_id_mapping: HashMap<String, String>,
}

impl<'a> ExecutionContext<'a> {
    pub fn new(admin_url: &str) -> ExecutionContext {
        let kong_cli = KongApiClient::build_with_url(admin_url);
        return ExecutionContext {
            api_names: Vec::new(),
            kong_cli: Box::new(kong_cli),
            support_api: false,
            support_service_route: false,
            service_name_id_mapping: HashMap::new(),
            route_name_id_mapping: HashMap::new(),
        };
    }
}

fn runc(tmpl_path: &str, admin_url: &str, is_wait: bool) -> Result<(), Error> {
    let mut context = ExecutionContext::new(admin_url);

    if is_wait {
        let mut is_connected = false;
        while !is_connected {
            is_connected = verify_kong_version(&mut context);
        }
    } else {
        verify_kong_version(&mut context);
    }

    let deserialized_conf = parse_template(tmpl_path, &context);

    match deserialized_conf {
        ConfFileStyle::Legacy(legacy_conf) => {
            init_apis(&mut context, &legacy_conf.apis);
            apply_plugins_to_api(&context, &legacy_conf.plugins);
        }
        ConfFileStyle::Suggested(suggested_conf) => {
            clear_before_init(&context);
            init_services(&mut context, &suggested_conf.services);
            init_routes(&mut context, suggested_conf.routes);
            apply_plugins_to_service_route(&context, &suggested_conf.plugins)
        }
        ConfFileStyle::IllegalFormat { msg } => {
            error!("invalid format: {}", msg);
            std::process::exit(1);
        }
    }

    Ok(())
}

fn verify_kong_version(context: &mut ExecutionContext) -> bool {
    let cli = &context.kong_cli;
    let retry_interval_ms = 5000;
    return match cli.get_node_info() {
        Err(why) => {
            error!("Could not reach Kong on {}; reason: {}", cli.base_url, why);
            error!("retry in {}ms", retry_interval_ms);
            sleep(Duration::from_millis(retry_interval_ms));
            false
        }
        Ok(kong_info) => {
            let kong_ver = &kong_info.version;
            info!("Kong version is {}", &kong_ver);
            if Version::parse(kong_ver) < Version::parse("0.13.0") {
                // kong under 0.13.X do not support service/route
                context.support_api = true;
            } else if Version::parse(kong_ver) < Version::parse("0.15.0") {
                // kong within 0.13.X and 0.14.X
                context.support_api = true;
                context.support_service_route = true;
            } else {
                // version >= 0.15.X, currently not supported.
                error!("kong version >= 0.15.X, currently not supported.");
                std::process::exit(1);
            }
            true
        }
    }
}

fn parse_template(tmpl_file_path: &str, context: &ExecutionContext) -> ConfFileStyle {
    let mut contents = String::new();

    match File::open(tmpl_file_path)
        .and_then(|mut file| file.read_to_string(&mut contents))
        .map_err(|io_err| Error::io(io_err))
        .and_then(|_| {
            if contents.contains("apis:\n") && contents.contains("services:\n")
                || (contents.contains("apis:\n") && contents.contains("services:\n")) {
                Ok(ConfFileStyle::IllegalFormat { msg: "yaml file cannot contains both 'apis' and 'services/routes' at the same time".to_string() })
            } else if contents.contains("apis:\n") {
                serde_yaml::from_str::<LegacyKongConf>(&replace_env_and_directive(&contents, context))
                    .map(|lkc| ConfFileStyle::Legacy(lkc))
            } else {
                serde_yaml::from_str::<KongConf>(&replace_env_and_directive(&contents, context))
                    .map(|kc| ConfFileStyle::Suggested(kc))
            }
        }) {
        Err(why) => {
            error!("invalid yaml file: {}", why);
            std::process::exit(1)
        }
        Ok(kong_conf) => kong_conf
    }
}

fn replace_env_and_directive(input: &str, context: &ExecutionContext) -> String {
    let after_env = _replace_env(input);
    debug!("full text after env replacement: \n{}", after_env);
    let after_d = _replace_directive(&after_env, context);
    debug!("full text after directive replacement: \n{}", after_d);
    return after_d;
}

fn _replace_directive(input: &str, context: &ExecutionContext) -> String {
    let dd_re = Regex::new(r"\{\{(.+?)\}\}").unwrap();

    let mut shit = HashMap::new();

    for caps in dd_re.captures_iter(input) {
        let cap_str = caps.get(1).unwrap().as_str();

        let vec: Vec<&str> = cap_str.splitn(2, ":").collect();

        match vec[0] {
            "k-upsert-consumer" => {
                debug!("create new consumer {}", vec[1]);
                shit.insert(cap_str.to_string(), context.kong_cli.init_guest_consumer(vec[1]));
            }
            _ => warn!("directive parsing error {}", vec[0]),
        }
    }
    let mut output = input.to_string();
    for (k, v) in shit.iter() {
        output = output.replace(&format!("{{{{{}}}}}", k), v);
    }
    return output;
}

fn _replace_env(input: &str) -> String {
    let env_re = Regex::new(r"\$\{(.+?)\}").unwrap();

    let mut tmp = HashMap::new();

    for caps in env_re.captures_iter(input) {
        let cap_str = caps.get(1).unwrap().as_str();
        let env_key = cap_str.to_string();

        match env::var(env_key) {
            Err(_) => {}
            Ok(env_value) => {
                tmp.insert(cap_str.to_string(), env_value);
            }
        };
    }
    let mut output = input.to_string();
    for (k, v) in tmp.iter() {
        output = output.replace(&format!("${{{}}}", k), v);
    }
    return output;
}

fn init_apis(context: &mut ExecutionContext, apis: &[ApiInfo]) {
    for api_info in apis {
        debug!("{:?}", api_info);
        let api_name = api_info.get("name").unwrap();
        context.api_names.push(api_name.clone());
        context.kong_cli.delete_api(&api_name);
        context.kong_cli.upsert_api(&api_name, api_info);
    }
    info!("finished loading APIs...");
    info!("=================================");
}

fn apply_plugins_to_api(context: &ExecutionContext, plugins: &[LegacyPluginInfo]) {
    for plugin_info in plugins {
        debug!("{:?}", plugin_info);
        let plugin_type = &plugin_info.plguin_type;
        let plugin_conf = &plugin_info.config;

        let target_apis: (LegacyPluginAppliedType, Option<Vec<String>>) =
            match &plugin_info.target_api as &str {
                "all" => (LegacyPluginAppliedType::ALL, None),
                "none" => (LegacyPluginAppliedType::NONE, None),
                others => (LegacyPluginAppliedType::SOME, Some(Vec::from_iter(others.split(",").map(String::from)))),
            };

        context.kong_cli.apply_plugin_to_api_legacy(plugin_type, target_apis, plugin_conf);
    }
    info!("finished loading plugins...");
    info!("=================================");
}

fn clear_before_init(context: &ExecutionContext) {
    info!("clear_before_init");
    context.kong_cli.delete_all_routes();
    context.kong_cli.delete_all_services();
}

fn init_services(context: &mut ExecutionContext, services: &[ServiceInfo]) {
    for service_info in services {
        let sid = context.kong_cli.add_service(&service_info).unwrap();
        let service_name = service_info.get("name").unwrap();
        context.service_name_id_mapping.insert(service_name.to_string(), sid);
    }
    info!("finished loading services...");
    info!("=================================");
}

fn init_routes(context: &mut ExecutionContext, routes: Vec<RouteInfo>) {
    for route_info in routes {
        let route_name = route_info.name.clone();
        let service_id = context.service_name_id_mapping.get(&route_info.apply_to).unwrap();
        let rid = context.kong_cli
            .add_route_to_service(service_id.to_string().clone(), route_info).unwrap();
        context.route_name_id_mapping.insert(route_name, rid);
    }
    info!("finished loading routes...");
    info!("=================================");
}

fn apply_plugins_to_service_route(context: &ExecutionContext, plugins: &[PluginInfo]) {
    let service_re = Regex::new(r"^s\[[-0-9a-zA-Z,]+\]$").unwrap();
    let route_re = Regex::new(r"^r\[[-0-9a-zA-Z,]+\]$").unwrap();

    for plugin_info in plugins {
        debug!("pluinInfo {:?}", plugin_info);

        let mut target = &plugin_info.target.clone();

        let plugin_target = if target == "global" {
            PluginTarget::GLOBAL
        } else if service_re.is_match(target) {
            let mut t = target.trim_left_matches("s[").to_string();
            let tm = t.len();
            t.truncate(tm - 1);
            let tmp = Vec::from_iter(t.split(",")
                .map(String::from)).iter().map(|s_name| {
                context.service_name_id_mapping.get(s_name).unwrap().clone()
            }).collect();
            debug!("plugin {} with service target {:?}", plugin_info.name, tmp);
            PluginTarget::SERVICES(tmp)
        } else if route_re.is_match(target) {
            let mut t = target.trim_left_matches("r[").to_string();
            let tm = t.len();
            t.truncate(tm - 1);
            let tmp = Vec::from_iter(t.split(",")
                .map(String::from)).iter().map(|r_name| {
                context.route_name_id_mapping.get(r_name).unwrap().clone()
            }).collect();
            debug!("plugin {} with route target {:?}", plugin_info.name, tmp);
            PluginTarget::Routes(tmp)
        } else {
            error!("invalid plugin defination: invalid target field, must one of global/services(s:[service-a,service-b,service-c])/routes(r:[route-a,route-b])");
            std::process::exit(1);
        };

        context.kong_cli.apply_plugin(plugin_target, plugin_info);
    }
    info!("finished loading plugins...");
    info!("=================================");
}