extern crate comrak;
extern crate glob;
extern crate handlebars;
#[macro_use]
extern crate lazy_static;
extern crate lazycell;
#[macro_use]
extern crate log;
extern crate rand;
extern crate regex;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate serde_json;
extern crate simple_logger;
extern crate syntect;
extern crate time;
extern crate toml;
extern crate typed_arena;
extern crate walkdir;

mod directives;
mod evaluator;
mod highlighter;
mod init;
mod lex;
mod markdown;
mod page;
mod parse;
mod theme;
mod toctree;

use std::collections::HashMap;
use std::convert::From;
use std::{env, mem, process};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use evaluator::Evaluator;
use page::{Page, Slug};
use toctree::TocTree;

#[derive(Debug)]
enum LinkError {
    UndefinedReference,
    TemplateError(handlebars::RenderError),
    IOError(io::Error),
}

impl From<handlebars::RenderError> for LinkError {
    fn from(orig: handlebars::RenderError) -> Self {
        LinkError::TemplateError(orig)
    }
}

impl From<io::Error> for LinkError {
    fn from(orig: io::Error) -> Self {
        LinkError::IOError(orig)
    }
}

#[derive(Deserialize)]
struct RawConfig {
    syntax_theme: Option<String>,
    theme: Option<PathBuf>,
    content_dir: Option<PathBuf>,
    output: Option<PathBuf>,
    templates: HashMap<String, String>,
    theme_constants: Option<serde_json::map::Map<String, serde_json::Value>>,
}

struct Project {
    verbose: bool,
    theme: theme::Theme,
    content_dir: PathBuf,
    output: PathBuf,
    templates: Vec<(glob::Pattern, String)>,
    theme_constants: serde_json::map::Map<String, serde_json::Value>,
    syntax_theme: String,

    pretty_url: bool,
}

impl Project {
    fn read_toml(path: &Path) -> Result<Project, ()> {
        let mut file = File::open(path).or(Err(()))?;
        let mut data = String::new();
        file.read_to_string(&mut data).or(Err(()))?;
        let config: RawConfig = toml::from_str(&data).or(Err(()))?;

        let theme_path = config.theme.ok_or(())?;
        let theme = theme::Theme::load(&theme_path)?;

        let path_patterns: Result<Vec<_>, ()> = config
            .templates
            .iter()
            .map(|(k, v)| {
                let pattern = match glob::Pattern::new(k) {
                    Ok(p) => p,
                    Err(_) => return Err(()),
                };
                Ok((pattern, v.to_owned()))
            })
            .collect();

        let path_patterns = path_patterns.or(Err(()))?;

        let syntax_theme = config
            .syntax_theme
            .unwrap_or_else(|| highlighter::DEFAULT_SYNTAX_THEME.to_owned());

        Ok(Project {
            verbose: false,
            theme,
            content_dir: config
                .content_dir
                .unwrap_or_else(|| PathBuf::from("content")),
            output: config.output.unwrap_or_else(|| PathBuf::from("build")),
            templates: path_patterns,
            theme_constants: config
                .theme_constants
                .unwrap_or_else(serde_json::map::Map::new),
            syntax_theme,
            pretty_url: true,
        })
    }

    fn build_file(&self, evaluator: &mut Evaluator, path: &Path) -> Result<Page, ()> {
        debug!("Compiling {}", evaluator.get_slug());

        let node = match evaluator.parser.parse(path) {
            Ok(n) => n,
            Err(msg) => {
                error!("Failed to parse '{}': {}", path.to_string_lossy(), msg);
                return Err(());
            }
        };

        let output = evaluator.evaluate(&node);

        let page = Page {
            source_path: path.to_owned(),
            slug: evaluator.get_slug().clone(),
            body: output,
            theme_config: evaluator.theme_config.clone(),
        };

        evaluator.reset();
        Ok(page)
    }

    fn link_file(
        &self,
        evaluator: &Evaluator,
        page: &Page,
        renderer: &mut theme::Renderer,
    ) -> Result<(), LinkError> {
        debug!("Linking {}", &page.slug);

        // Find the template that matches this path
        let template_name = self.templates
            .iter()
            .find(|&&(ref pat, _)| pat.matches_path(&page.source_path))
            .map(|&(_, ref name)| name.as_ref())
            .unwrap_or("default");

        let new_body = match evaluator.substitute(page) {
            Ok(s) => s,
            Err(_) => {
                return Err(LinkError::UndefinedReference);
            }
        };

        let rendered = renderer.render(template_name, &self.theme_constants, page, &new_body)?;
        let output_path = page.slug.create_output_path(&self.output, self.pretty_url);
        let output_dir = output_path.parent().expect("Couldn't get output directory");

        fs::create_dir_all(output_dir)?;
        let mut file = File::create(&output_path)?;
        file.write_all(rendered.as_bytes())?;

        Ok(())
    }

    fn build_project(&self, evaluator: &mut Evaluator) {
        let mut pending_pages = vec![];
        let mut titles = HashMap::new();

        for entry in walkdir::WalkDir::new(&self.content_dir) {
            let entry = entry.expect("Failed to walk dir");
            if !entry.file_type().is_file() {
                continue;
            }

            if entry.path().extension() != Some("rocket".as_ref()) {
                continue;
            }

            let path = entry.path();
            let slug = path.strip_prefix(&self.content_dir)
                .expect("Failed to get output path");
            let dir = slug.parent().unwrap();
            let stem = slug.file_stem().unwrap();
            let slug = Slug::new(dir.join(stem).to_string_lossy().as_ref().to_owned());
            evaluator.set_slug(slug);

            match self.build_file(evaluator, path) {
                Ok(page) => {
                    titles.insert(page.slug.to_owned(), page.title());
                    pending_pages.push(page);
                }
                Err(_) => {
                    error!("Failed to build {}", path.to_string_lossy());
                }
            }
        }

        let mut toctree = mem::replace(&mut evaluator.toctree, TocTree::new_empty());
        toctree.finish(titles);

        let mut renderer =
            theme::Renderer::new(&self.theme, toctree).expect("Failed to construct renderer");
        for page in &pending_pages {
            self.link_file(evaluator, page, &mut renderer)
                .expect("Failed to link page");
        }
    }
}

fn build(verbose: bool) {
    let mut config =
        Project::read_toml(Path::new("config.toml")).expect("Failed to open config.toml");

    config.verbose = verbose;

    let mut evaluator = Evaluator::new_with_options(&config.syntax_theme);
    evaluator.register_prelude("md", Box::new(directives::Markdown));
    evaluator.register_prelude("table", Box::new(directives::Dummy));
    evaluator.register_prelude("version", Box::new(directives::Version::new("3.4.0")));
    evaluator.register_prelude(
        "note",
        Box::new(directives::Admonition::new("Note", "note")),
    );
    evaluator.register_prelude(
        "warning",
        Box::new(directives::Admonition::new("Warning", "warning")),
    );
    evaluator.register_prelude("define-template", Box::new(directives::DefineTemplate));
    evaluator.register_prelude("definition-list", Box::new(directives::DefinitionList));
    evaluator.register_prelude("steps", Box::new(directives::Steps));
    evaluator.register_prelude("concat", Box::new(directives::Concat));
    evaluator.register_prelude("include", Box::new(directives::Include));
    evaluator.register_prelude("import", Box::new(directives::Import));
    evaluator.register_prelude("null", Box::new(directives::Dummy));
    evaluator.register_prelude("let", Box::new(directives::Let));
    evaluator.register_prelude("define", Box::new(directives::Define));
    evaluator.register_prelude("theme-config", Box::new(directives::ThemeConfig));
    evaluator.register_prelude("toctree", Box::new(directives::TocTree));
    evaluator.register_prelude("define-ref", Box::new(directives::RefDefDirective));
    evaluator.register_prelude("ref", Box::new(directives::RefDirective));

    evaluator.register_prelude("h1", Box::new(directives::Heading::new(1)));
    evaluator.register_prelude("h2", Box::new(directives::Heading::new(2)));
    evaluator.register_prelude("h3", Box::new(directives::Heading::new(3)));
    evaluator.register_prelude("h4", Box::new(directives::Heading::new(4)));
    evaluator.register_prelude("h5", Box::new(directives::Heading::new(5)));
    evaluator.register_prelude("h6", Box::new(directives::Heading::new(6)));

    let start_time = time::precise_time_ns();
    config.build_project(&mut evaluator);

    info!(
        "Took {} seconds",
        (time::precise_time_ns() - start_time) as f64 / (f64::from(1_000_000_000))
    );
}

const DESCRIPTION_BUILD: &'static str =
    "Build the Rocket project in the current working directory.";
const DESCRIPTION_NEW: &'static str = "Create an empty Rocket project.";
const HELP_VERBOSE: &'static str = "Increase logging verbosity.";

enum ArgMode {
    Root,
    New,
    Build,
}

fn main() {
    let args = env::args().skip(1);
    let mut verbose = false;
    let mut new_name: Option<String> = None;
    let mut mode = ArgMode::Root;

    let help = |code| -> ! {
        println!("Usage:\n  rocket [-h, OPTS...] {{ new | build }} ...\n");
        println!("Description:\n  The Rocket documentation build system.\n");
        println!(
            "Subcommands:\n  new\n    {}\n  build\n    {}\n",
            DESCRIPTION_NEW,
            DESCRIPTION_BUILD
        );
        println!("Optional arguments:");
        println!("  --help, -h\n    Print this message and exit.\n");
        println!("  --version, -V\n    Print version string and exit.\n");

        process::exit(code);
    };

    let help_build = |code| -> ! {
        println!("Usage:\n  rocket build [-h, OPTS...]\n");
        println!("Description:\n  {}\n", DESCRIPTION_BUILD);
        println!("Optional arguments:");
        println!("  --verbose, -v\n    {}\n", HELP_VERBOSE);
        println!("  --help, -h\n    Print this message and exit.\n");

        process::exit(code);
    };

    let help_new = |code| -> ! {
        println!("Usage:\n  rocket new [-h, OPTS...] name\n");
        println!("Description:\n  {}\n", DESCRIPTION_NEW);
        println!("Positional arguments:\n  name\n    The name of the project to create.\n");
        println!("Optional arguments:");
        println!("  --verbose, -v\n    {}\n", HELP_VERBOSE);
        println!("  --help, -h\n    Print this message and exit.\n");

        process::exit(code);
    };

    for arg in args {
        match mode {
            ArgMode::Root => match arg.as_ref() {
                "-h" | "--help" => help(0),
                "-V" | "--version" => {
                    println!(
                        "{}",
                        option_env!("CARGO_PKG_VERSION").unwrap_or("<unknown>")
                    );
                    return;
                }
                "-v" | "--verbose" => verbose = true,
                "build" => mode = ArgMode::Build,
                "new" => mode = ArgMode::New,
                _ => help(1),
            },
            ArgMode::New => {
                let alphanumeric = arg.chars().all(|c| c.is_alphabetic() || c.is_numeric());
                match arg.as_ref() {
                    "-h" | "--help" => help_new(0),
                    "-v" | "--verbose" => verbose = true,
                    n if alphanumeric => new_name = Some(n.to_owned()),
                    _ => help_new(1),
                }
            }
            ArgMode::Build => match arg.as_ref() {
                "-h" | "--help" => help_build(0),
                "-v" | "--verbose" => verbose = true,
                _ => help_build(1),
            },
        }
    }

    let loglevel = if verbose {
        log::LogLevel::Debug
    } else {
        log::LogLevel::Info
    };

    simple_logger::init_with_level(loglevel).expect("Failed to initialize logger");

    match mode {
        ArgMode::Root => help(1),
        ArgMode::New => init::init(&new_name.unwrap_or_else(|| help_new(1))),
        ArgMode::Build => build(verbose),
    }
}
