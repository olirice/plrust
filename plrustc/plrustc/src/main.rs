#![feature(rustc_private)]
extern crate rustc_ast;
extern crate rustc_driver;
extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_interface;

extern crate rustc_lint;
extern crate rustc_lint_defs;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;

use once_cell::sync::Lazy;
use rustc_driver::Callbacks;
use rustc_interface::interface;
use rustc_session::config::ErrorOutputType;
use rustc_session::early_error;
use rustc_session::parse::ParseSess;
use rustc_span::source_map::FileLoader;
use rustc_span::Symbol;
use std::path::Path;

const PLRUSTC_USER_CRATE_NAME: &str = "PLRUSTC_USER_CRATE_NAME";
const PLRUSTC_USER_CRATE_ALLOWED_SOURCE_PATHS: &str = "PLRUSTC_USER_CRATE_ALLOWED_SOURCE_PATHS";

const PLRUSTC_VERSION: &str = env!("CARGO_PKG_VERSION");

mod lints;

struct PlrustcCallbacks {
    lints_enabled: bool,
    config: PlrustcConfig,
}

impl Callbacks for PlrustcCallbacks {
    fn config(&mut self, config: &mut interface::Config) {
        let cfg = self.config.clone();
        config.parse_sess_created = Some(Box::new(move |parse_sess| {
            cfg.track(parse_sess);
        }));
        if self.lints_enabled {
            let previous = config.register_lints.take();
            config.register_lints = Some(Box::new(move |sess, lint_store| {
                if let Some(previous) = &previous {
                    (previous)(sess, lint_store);
                }
                lints::register(lint_store, sess);
            }));
        }
    }
}

// TODO: eventually we can replace this with:
// rustc_driver::install_ice_hook("https://github.com/tcdi/plrust/issues/new", |_| ());
fn install_ice_hook() {
    fn report_plrustc_ice(info: &std::panic::PanicInfo<'_>, bug_report_url: &str) {
        // Invoke the default panic handler to print the message and (possibly) a back trace
        (*PANIC_HOOK)(info);
        // Separate output with an empty line
        eprintln!();

        let fallback_bundle = rustc_errors::fallback_fluent_bundle(
            rustc_driver::DEFAULT_LOCALE_RESOURCES.into(),
            false,
        );
        let emitter = Box::new(rustc_errors::emitter::EmitterWriter::stderr(
            rustc_errors::ColorConfig::Auto,
            None,
            None,
            fallback_bundle,
            false,
            false,
            None,
            false,
            false,
            rustc_errors::TerminalUrl::No,
        ));
        let handler = rustc_errors::Handler::with_emitter(true, None, emitter);

        // Don't need to print anything extra for ExplicitBug
        if !info.payload().is::<rustc_errors::ExplicitBug>() {
            let mut d = rustc_errors::Diagnostic::new(rustc_errors::Level::Bug, "unexpected panic");
            handler.emit_diagnostic(&mut d);
        }
        handler.note_without_error("`plrustc` unexpectedly panicked. This is probably a bug.");
        handler.note_without_error(&format!("Please file a bug report at <{bug_report_url}>"));
        handler.note_without_error(&format!("plrustc version: {PLRUSTC_VERSION}"));

        // If backtraces are enabled, also print the query stack
        let backtrace = std::env::var_os("RUST_BACKTRACE").map_or(false, |x| &x != "0");

        let num_frames = if backtrace { None } else { Some(2) };

        interface::try_print_query_stack(&handler, num_frames);
    }

    type PanicCallback = Box<dyn Fn(&std::panic::PanicInfo<'_>) + Sync + Send + 'static>;
    static PANIC_HOOK: Lazy<PanicCallback> = Lazy::new(|| {
        let hook = std::panic::take_hook();
        let bug_report_url = "https://github.com/tcdi/plrust/issues/new";
        std::panic::set_hook(Box::new(|info| report_plrustc_ice(info, bug_report_url)));
        hook
    });
    Lazy::force(&PANIC_HOOK);
}

fn main() {
    install_ice_hook();
    rustc_driver::init_rustc_env_logger();
    std::process::exit(rustc_driver::catch_with_exit_code(move || {
        let args = rustc_driver::args::arg_expand_all(&std::env::args().collect::<Vec<_>>());
        let config = PlrustcConfig::from_env_and_args(&args);
        run_compiler(
            args,
            &mut PlrustcCallbacks {
                // FIXME SOMEDAY: check caplints?
                lints_enabled: true,
                config,
            },
        );
    }))
}

#[derive(Debug, Clone)]
struct PlrustcConfig {
    // If `--crate-name` was provided, that.
    crate_name_arg: Option<String>,
    // PLRUSTC_USER_CRATE_NAME
    plrust_user_crate_name: Option<String>,
    // PLRUSTC_USER_CRATE_ALLOWED_SOURCE_PATHS
    plrust_user_crate_allowed_source_paths: Option<String>,
}

impl PlrustcConfig {
    fn from_env_and_args(args: &[String]) -> Self {
        PlrustcConfig {
            crate_name_arg: arg_value(args, "--crate-name").map(|s| s.to_string()),
            plrust_user_crate_name: std::env::var(PLRUSTC_USER_CRATE_NAME).ok(),
            plrust_user_crate_allowed_source_paths: std::env::var(
                PLRUSTC_USER_CRATE_ALLOWED_SOURCE_PATHS,
            )
            .ok(),
        }
    }

    fn compiling_user_crate(&self) -> bool {
        if let (Some(current), Some(user)) = (
            self.crate_name_arg.as_deref(),
            self.plrust_user_crate_name.as_deref(),
        ) {
            current == user
        } else {
            false
        }
    }

    fn track(&self, parse_sess: &mut ParseSess) {
        if self.compiling_user_crate() {
            parse_sess.env_depinfo.lock().insert((
                Symbol::intern(PLRUSTC_USER_CRATE_NAME),
                self.plrust_user_crate_name.as_deref().map(Symbol::intern),
            ));
            parse_sess.env_depinfo.lock().insert((
                Symbol::intern(PLRUSTC_USER_CRATE_ALLOWED_SOURCE_PATHS),
                self.plrust_user_crate_allowed_source_paths
                    .as_deref()
                    .map(Symbol::intern),
            ));
        }
    }

    fn make_file_loader(&self) -> Box<dyn FileLoader + Send + Sync> {
        if !self.compiling_user_crate() {
            Box::new(ErrorHidingFileLoader)
        } else {
            let Some(allowed) = self.plrust_user_crate_allowed_source_paths.as_deref() else {
                early_error(
                    ErrorOutputType::default(),
                    &format!(
                        "if `{PLRUSTC_USER_CRATE_NAME}` is provided, \
                        then `{PLRUSTC_USER_CRATE_ALLOWED_SOURCE_PATHS}` should also be provided",
                    ),
                );
            };

            // Should we add the cargo registry? The sysroot? Hm...
            let allowed_source_dirs = std::env::split_paths(allowed).filter_map(|path| {
                if !path.is_absolute() {
                    early_error(
                        ErrorOutputType::default(),
                        &format!("`{PLRUSTC_USER_CRATE_ALLOWED_SOURCE_PATHS}` contains relative path: {allowed:?}"),
                    );
                }
                let path = path.canonicalize().ok()?;
                let Some(pathstr) = path.to_str() else {
                    early_error(
                        ErrorOutputType::default(),
                        &format!("`{PLRUSTC_USER_CRATE_ALLOWED_SOURCE_PATHS}` contains non-UTF-8 path: {allowed:?}"),
                    );
                };
                Some(pathstr.to_owned())
            }).collect::<Vec<String>>();
            if allowed_source_dirs.is_empty() {
                early_error(
                    ErrorOutputType::default(),
                    &format!(
                        "`{PLRUSTC_USER_CRATE_ALLOWED_SOURCE_PATHS}` was provided but contained no paths \
                        which exist: {allowed:?}",
                    ),
                );
            }

            Box::new(PlrustcFileLoader {
                allowed_source_dirs,
            })
        }
    }
}

fn arg_value<'a, T: AsRef<str>>(args: &'a [T], find_arg: &str) -> Option<&'a str> {
    let mut args = args.iter().map(|s| s.as_ref());
    while let Some(arg) = args.next() {
        let mut arg = arg.splitn(2, '=');
        if arg.next() != Some(find_arg) {
            continue;
        }

        if let Some(a) = arg.next().or_else(|| args.next()) {
            return Some(a);
        }
    }
    None
}

struct ErrorHidingFileLoader;

fn replacement_error() -> std::io::Error {
    // Unix-ism, but non-unix could use `io::Error::from(ErrorKind::NotFound)`
    // or something like that.
    std::io::Error::from_raw_os_error(libc::ENOENT)
}

impl FileLoader for ErrorHidingFileLoader {
    fn file_exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn read_file(&self, path: &Path) -> std::io::Result<String> {
        std::fs::read_to_string(path).map_err(|_| {
            // TODO: Should there be a way to preserve errors for debugging?
            replacement_error()
        })
    }

    fn read_binary_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        std::fs::read(path).map_err(|_| {
            // TODO: Should there be a way to preserve errors for debugging?
            replacement_error()
        })
    }
}

struct PlrustcFileLoader {
    allowed_source_dirs: Vec<String>,
}

impl PlrustcFileLoader {
    fn is_inside_allowed_dir(&self, p: &Path) -> bool {
        let Ok(child) = p.canonicalize() else {
            // If we can't canonicalize it, we can't tell.
            return false;
        };
        self.allowed_source_dirs.iter().any(|root| {
            if let Ok(root) = Path::new(root).canonicalize() {
                child.starts_with(&root)
            } else {
                false
            }
        })
    }
}

impl FileLoader for PlrustcFileLoader {
    fn file_exists(&self, path: &Path) -> bool {
        self.is_inside_allowed_dir(path) && ErrorHidingFileLoader.file_exists(path)
    }

    fn read_file(&self, path: &Path) -> std::io::Result<String> {
        if path.exists() && !path.is_dir() && self.is_inside_allowed_dir(path) {
            ErrorHidingFileLoader.read_file(path)
        } else {
            Err(replacement_error())
        }
    }

    fn read_binary_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        if path.exists() && !path.is_dir() && self.is_inside_allowed_dir(path) {
            ErrorHidingFileLoader.read_binary_file(path)
        } else {
            Err(replacement_error())
        }
    }
}

fn run_compiler(mut args: Vec<String>, callbacks: &mut PlrustcCallbacks) -> ! {
    args.splice(1..1, std::iter::once("--cfg=plrustc".to_string()));

    std::process::exit(rustc_driver::catch_with_exit_code(move || {
        let file_loader = callbacks.config.make_file_loader();
        let mut driver = rustc_driver::RunCompiler::new(&args, callbacks);
        driver.set_file_loader(Some(file_loader));
        driver.run()
    }));
}
