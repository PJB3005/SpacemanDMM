//! **dm2doxy** is a Doxygen filter for DreamMaker/BYOND codebases.
//!
//! Because DreamMaker codebases are only reasonably parsed in a solid chunk,
//! we operate by parsing the entire environment and then saving out the doc
//! comments alongside line-number-accurate analogues of the DM definitions.

extern crate dreammaker as dm;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::{io, fs};
use std::rc::Rc;

use dm::*;

/// Entry point - invoke `.dm` or `.dme` driver based on command-line filename.
fn main() {
    let mut args = std::env::args_os();
    let _ = args.next();  // discard executable
    let fname = match args.next() {
        Some(arg) => PathBuf::from(arg),
        None => return eprintln!("specify filename"),
    };

    if let Err(e) = match fname.extension().and_then(|s| s.to_str()) {
        Some("dme") => dme,
        Some("dm") => dm,
        other => return eprintln!("bad extension: {:?}", other),
    }(fname) {
        eprintln!("    error: {}", e);
        std::process::exit(1);
    }
}

/// The scoping operator Doxygen is expecting.
const SCOPE: &str = "::";

/// Map from real path to temporary file.
fn tempfile(path: &Path) -> PathBuf {
    // TODO: replace backslashes as well
    Path::new("dm2doxy").join(path.display().to_string().replace("/", "$").replace(".dm", ".."))
}

/// ---------------------------------------------------------------------------
/// `.dm` files - read temp files
fn dm(fname: PathBuf) -> Result<(), Box<std::error::Error>> {
    use std::io::{Read, Write};

    let path = tempfile(fname.strip_prefix(std::env::current_dir()?)?);
    let mut contents = Vec::new();
    fs::File::open(path)?.read_to_end(&mut contents)?;
    // TODO: delete the tempfile
    let stdout = io::stdout();
    stdout.lock().write_all(&contents)?;
    Ok(())
}

/// ---------------------------------------------------------------------------
/// `.dme` files - parse the environment, collate definitions, write temp files
fn dme(fname: PathBuf) -> Result<(), Box<std::error::Error>> {
    use std::io::Write;

    // parse the environment
    eprintln!("parsing {}", fname.display());
    let (tx, rx) = std::sync::mpsc::channel();
    let ctx = &Context::default();
    let mut pp = preprocessor::Preprocessor::new(ctx, fname)?;
    pp.save_comments(tx);
    let objtree = parser::parse(ctx, indents::IndentProcessor::new(ctx, pp));

    // index all definitions
    let mut defs = Definitions::default();
    let mut extends = BTreeMap::new();

    while let Ok(comment) = rx.try_recv() {
        defs.push(comment.location, Definition { comment: true, ..Definition::new(&Rc::from(""), comment.text) });
    }

    objtree.root().recurse(&mut |ty| {
        // start with the class and what it extends from, if anything
        let class: Rc<str>;
        if ty.path.is_empty() {
            class = Rc::from("");
        } else {
            class = Rc::from(path(ty));
            if let Some(parent) = ty.parent_type() {
                if !parent.path.is_empty() {
                    extends.insert(class.clone(), path(parent));
                }
            }
            defs.push(ty.location, Definition::new(&class, String::new()));
        };

        // list all the vars since these usually come first
        for (name, var) in ty.vars.iter() {
            let decl = match &var.declaration {
                None => String::new(),
                Some(decl) => decl.var_type.type_path.join(SCOPE),
            };
            defs.push(var.value.location, Definition::new(&class, format!("{} {};", decl, name)));
        }

        // list all the procs
        for (name, proc) in ty.procs.iter() {
            let decl = match &proc.declaration {
                None => "",
                Some(decl) => if decl.is_verb { "verb" } else { "proc" },
            };

            // TODO: ensure doc comments on the proc end up on the proc and not
            // the class, if the proc gets preceded by a class opener.
            defs.push(proc.value.location, Definition::new(&class, format!("{} {}(", decl, name)));
            let mut sep = "";
            for param in proc.value.parameters.iter() {
                defs.push(proc.value.location, Definition::new(&class, format!("{}{}{}", sep, param.path.join(SCOPE), param.name)));
                sep = ", ";
            }
            defs.push(proc.value.location, Definition::new(&class, "){}".to_owned()));
        }
    });

    // collate definitions into files and lines
    let mut map: BTreeMap<FileId, BTreeMap<u32, Vec<String>>> = BTreeMap::new();
    let mut last: Option<(Location, Rc<str>)> = None;

    for (location, def_vec) in defs.map {
        for def in def_vec {
            // if we're in a different file or class than before
            if !def.comment {
                if let Some((last_loc, last_class)) = last.take() {
                    if def.class != last_class || location.file != last_loc.file {
                        // close the previous class
                        if !last_class.is_empty() {
                            map
                                .entry(last_loc.file).or_default()
                                .entry(last_loc.line).or_default()
                                .push("}".to_owned());
                        }

                        // open the current class
                        let dest = map
                            .entry(location.file).or_default()
                            .entry(location.line).or_default();
                        if !def.class.is_empty() {
                            dest.push(format!("class {}", &def.class));
                            if let Some(extends) = extends.remove(&def.class) {
                                dest.push(format!(" extends {}", extends));
                            }
                            dest.push("{".to_owned());
                        }
                    }
                }
                last = Some((location, def.class));
            }

            // write the current entry
            map
                .entry(location.file).or_default()
                .entry(location.line).or_default()
                .push(def.bit);
        }
    }

    if let Some((location, class)) = last.take() {
        if !class.is_empty() {
            map
                .entry(location.file).or_default()
                .entry(location.line).or_default()
                .push("}".to_owned());
        }
    }

    // save collated files
    for (id, lines) in map {
        let path = ctx.file_path(id);
        let mut f: Box<io::Write>;
        if id == FileId::builtins() {
            f = Box::new(io::stdout());
        } else if ctx.get_file(&path).is_some() {
            let tempfile = tempfile(&path);
            if let Some(p) = tempfile.parent() {
                fs::create_dir_all(p)?;
            }
            f = Box::new(fs::File::create(tempfile)?);
        } else {
            continue;
        }

        let mut last = 1;
        let mut total_items = 0;
        let num_lines = lines.len();
        for (line_number, items) in lines {
            for _ in last..line_number {
                writeln!(f)?;
            }
            last = line_number;
            for item in items {
                write!(f, "{}", item)?;
                total_items += 1;
            }
        }
        writeln!(f)?;
        eprintln!("    {}: {} lines with {} items", path.display(), num_lines, total_items);
    }

    Ok(())
}

fn path(ty: objtree::TypeRef) -> String {
    if ty.path.is_empty() {
        "globals".to_owned()
    } else {
        ty.path[1..].replace("/", SCOPE)
    }
}

#[derive(Default)]
struct Definitions {
    map: BTreeMap<Location, Vec<Definition>>,
}

impl Definitions {
    fn push(&mut self, loc: Location, def: Definition) {
        self.map.entry(loc).or_default().push(def);
    }
}

struct Definition {
    class: Rc<str>,
    bit: String,
    comment: bool,
}

impl Definition {
    fn new(class: &Rc<str>, bit: String) -> Definition {
        Definition { class: class.clone(), bit, comment: false }
    }
}
