//! Dynamic dependencies (`dyndep`): extra inputs and outputs discovered by
//! running part of the build.

use std::collections::BTreeMap;

use crate::canon::canonicalize_path;
use crate::disk::DiskInterface;
use crate::error::{Error, Result};
use crate::eval::{EvalString, MapEnv};
use crate::lexer::{Lexer, Token};
use crate::state::{EdgeId, NodeId, State};
use crate::version::parse_version;

/// Dynamically discovered dependency information for one edge.
#[derive(Clone, Debug, Default)]
pub struct Dyndeps {
    /// Set once the information has been applied to its edge.
    pub used: bool,
    /// Whether the dyndep file asked for `restat` behaviour.
    pub restat: bool,
    /// Extra implicit inputs.
    pub implicit_inputs: Vec<NodeId>,
    /// Extra implicit outputs.
    pub implicit_outputs: Vec<NodeId>,
}

/// The contents of one dyndep file: a map from edge to its new dependencies.
#[derive(Clone, Debug, Default)]
pub struct DyndepFile {
    /// Entries keyed by edge, in manifest order.
    pub entries: BTreeMap<EdgeId, Dyndeps>,
}

/// Parse the dyndep file at `path`, creating any new nodes in `state`.
pub fn parse_dyndep_file(
    state: &mut State,
    disk: &dyn DiskInterface,
    path: &str,
) -> Result<DyndepFile> {
    let contents = match disk.read_file(path)? {
        Some(c) => c,
        None => {
            return Err(Error::Manifest(format!(
                "loading '{path}': No such file or directory"
            )));
        }
    };
    parse_dyndep_text(state, path, &contents)
}

/// Parse dyndep text that has already been read.
pub fn parse_dyndep_text(state: &mut State, filename: &str, input: &[u8]) -> Result<DyndepFile> {
    let mut lexer = Lexer::new(filename, input);
    let mut ddf = DyndepFile::default();
    let mut have_version = false;

    loop {
        let token = lexer.read_token();
        match token {
            Token::Build => {
                if !have_version {
                    return Err(lexer.error("expected 'ninja_dyndep_version = ...'"));
                }
                parse_edge(state, &mut lexer, &mut ddf)?;
            }
            Token::Ident => {
                lexer.unread_token();
                if have_version {
                    return Err(lexer.error(&format!("unexpected {}", token.name())));
                }
                parse_dyndep_version(&mut lexer)?;
                have_version = true;
            }
            Token::Error => {
                let msg = lexer.describe_last_error();
                return Err(lexer.error(&msg));
            }
            Token::Eof => {
                if !have_version {
                    return Err(lexer.error("expected 'ninja_dyndep_version = ...'"));
                }
                return Ok(ddf);
            }
            Token::Newline => {}
            other => return Err(lexer.error(&format!("unexpected {}", other.name()))),
        }
    }
}

fn parse_let(lexer: &mut Lexer<'_>) -> Result<(String, EvalString)> {
    let key = match lexer.read_ident() {
        Some(k) => k,
        None => return Err(lexer.error("expected variable name")),
    };
    lexer.expect_token(Token::Equals)?;
    let mut value = EvalString::new();
    lexer.read_var_value(&mut value)?;
    Ok((key, value))
}

fn parse_dyndep_version(lexer: &mut Lexer<'_>) -> Result<()> {
    let (name, value) = parse_let(lexer)?;
    if name != "ninja_dyndep_version" {
        return Err(lexer.error("expected 'ninja_dyndep_version = ...'"));
    }
    let mut env = MapEnv::new();
    let version = value.evaluate(&mut env);
    let (major, minor) = parse_version(&version);
    if major != 1 || minor != 0 {
        return Err(lexer.error(&format!("unsupported 'ninja_dyndep_version = {version}'")));
    }
    Ok(())
}

fn parse_edge(state: &mut State, lexer: &mut Lexer<'_>, ddf: &mut DyndepFile) -> Result<()> {
    let mut env = MapEnv::new();

    // Exactly one explicit output, which must already have a build statement.
    let edge = {
        let mut out0 = EvalString::new();
        lexer.read_path(&mut out0)?;
        if out0.is_empty() {
            return Err(lexer.error("expected path"));
        }
        let mut path = out0.evaluate(&mut env);
        if path.is_empty() {
            return Err(lexer.error("empty path"));
        }
        canonicalize_path(&mut path);
        let node = state.lookup_node(&path);
        let edge = node.and_then(|n| state.node(n).in_edge());
        let Some(edge) = edge else {
            return Err(lexer.error(&format!("no build statement exists for '{path}'")));
        };
        if ddf.entries.contains_key(&edge) {
            return Err(lexer.error(&format!("multiple statements for '{path}'")));
        }
        ddf.entries.insert(edge, Dyndeps::default());
        edge
    };

    // No further explicit outputs.
    {
        let mut out = EvalString::new();
        lexer.read_path(&mut out)?;
        if !out.is_empty() {
            return Err(lexer.error("explicit outputs not supported"));
        }
    }

    // Implicit outputs.
    let mut outs: Vec<EvalString> = Vec::new();
    if lexer.peek_token(Token::Pipe) {
        loop {
            let mut out = EvalString::new();
            lexer.read_path(&mut out)?;
            if out.is_empty() {
                break;
            }
            outs.push(out);
        }
    }

    lexer.expect_token(Token::Colon)?;

    match lexer.read_ident() {
        Some(name) if name == "dyndep" => {}
        _ => return Err(lexer.error("expected build command name 'dyndep'")),
    }

    // No explicit inputs.
    {
        let mut input = EvalString::new();
        lexer.read_path(&mut input)?;
        if !input.is_empty() {
            return Err(lexer.error("explicit inputs not supported"));
        }
    }

    // Implicit inputs.
    let mut ins: Vec<EvalString> = Vec::new();
    if lexer.peek_token(Token::Pipe) {
        loop {
            let mut input = EvalString::new();
            lexer.read_path(&mut input)?;
            if input.is_empty() {
                break;
            }
            ins.push(input);
        }
    }

    if lexer.peek_token(Token::Pipe2) {
        return Err(lexer.error("order-only inputs not supported"));
    }

    lexer.expect_token(Token::Newline)?;

    let mut restat = false;
    if lexer.peek_token(Token::Indent) {
        let (key, val) = parse_let(lexer)?;
        if key != "restat" {
            return Err(lexer.error("binding is not 'restat'"));
        }
        restat = !val.evaluate(&mut env).is_empty();
    }

    let mut implicit_inputs = Vec::with_capacity(ins.len());
    for input in &ins {
        let mut path = input.evaluate(&mut env);
        if path.is_empty() {
            return Err(lexer.error("empty path"));
        }
        let slash_bits = canonicalize_path(&mut path);
        implicit_inputs.push(state.get_node(&path, slash_bits));
    }

    let mut implicit_outputs = Vec::with_capacity(outs.len());
    for out in &outs {
        let mut path = out.evaluate(&mut env);
        if path.is_empty() {
            return Err(lexer.error("empty path"));
        }
        let slash_bits = canonicalize_path(&mut path);
        implicit_outputs.push(state.get_node(&path, slash_bits));
    }

    let entry = ddf.entries.get_mut(&edge).expect("just inserted");
    entry.restat = restat;
    entry.implicit_inputs = implicit_inputs;
    entry.implicit_outputs = implicit_outputs;
    Ok(())
}

/// Load the dyndep file named by `node` and apply it to the edges that declare
/// it, returning what was loaded so the build plan can be updated.
pub fn load_dyndeps(
    state: &mut State,
    disk: &dyn DiskInterface,
    node: NodeId,
) -> Result<DyndepFile> {
    // The file is being loaded now, so it is no longer pending.
    state.node_mut(node).dyndep_pending = false;

    let path = state.node(node).path().to_string();
    let mut ddf = parse_dyndep_file(state, disk, &path)?;

    let out_edges: Vec<EdgeId> = state.node(node).out_edges().to_vec();
    for edge in out_edges {
        if state.edge(edge).dyndep() != Some(node) {
            continue;
        }
        let dyndeps = match ddf.entries.get_mut(&edge) {
            Some(d) => {
                d.used = true;
                d.clone()
            }
            None => {
                let out = state.node(state.edge(edge).outputs()[0]).path().to_string();
                return Err(Error::build(format!(
                    "'{out}' not mentioned in its dyndep file '{path}'"
                )));
            }
        };
        update_edge(state, edge, &dyndeps)?;
    }

    // Every statement in the file must belong to an edge that asked for it.
    for (edge, dyndeps) in &ddf.entries {
        if !dyndeps.used {
            let out = state.node(state.edge(*edge).outputs()[0]).path().to_string();
            return Err(Error::build(format!(
                "dyndep file '{path}' mentions output '{out}' whose build statement does not \
                 have a dyndep binding for the file"
            )));
        }
    }

    Ok(ddf)
}

fn update_edge(state: &mut State, edge: EdgeId, dyndeps: &Dyndeps) -> Result<()> {
    // ninja records this by adding a `restat = 1` binding to the edge's
    // environment; we keep it on the edge so we never mutate a scope that
    // other edges may share.
    if dyndeps.restat {
        state.edge_mut(edge).dyndep_restat = true;
    }

    // New implicit outputs.
    for &n in &dyndeps.implicit_outputs {
        if state.node(n).in_edge().is_some() {
            return Err(Error::build(format!(
                "multiple rules generate {}",
                state.node(n).path()
            )));
        }
    }
    {
        let count = dyndeps.implicit_outputs.len();
        let e = state.edge_mut(edge);
        e.outputs.extend_from_slice(&dyndeps.implicit_outputs);
        e.implicit_outs += count;
    }
    for &n in &dyndeps.implicit_outputs {
        state.node_mut(n).in_edge = Some(edge);
    }

    // New implicit inputs go before the order-only inputs.
    {
        let e = state.edge(edge);
        let pos = e.inputs.len() - e.order_only_deps;
        let count = dyndeps.implicit_inputs.len();
        let e = state.edge_mut(edge);
        e.inputs
            .splice(pos..pos, dyndeps.implicit_inputs.iter().copied());
        e.implicit_deps += count;
    }
    for &n in &dyndeps.implicit_inputs {
        state.node_mut(n).out_edges.push(edge);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::MemDisk;
    use crate::parse::{ManifestParser, ParserOptions};

    fn setup() -> (State, MemDisk) {
        let disk = MemDisk::new();
        let mut state = State::new();
        {
            let mut p = ManifestParser::new(
                &mut state,
                &disk,
                ParserOptions {
                    quiet: true,
                    ..Default::default()
                },
            );
            p.parse_text(
                "input",
                b"rule r\n  command = touch $out\n  dyndep = dd\n\nbuild out: r in | dd\n",
            )
            .unwrap();
        }
        (state, disk)
    }

    #[test]
    fn loads_implicit_inputs_and_outputs() {
        let (mut state, disk) = setup();
        disk.create(
            "dd",
            "ninja_dyndep_version = 1\nbuild out | imp_out: dyndep | imp_in\n",
        );
        let dd = state.lookup_node("dd").unwrap();
        load_dyndeps(&mut state, &disk, dd).unwrap();

        let edge = crate::state::EdgeId(0);
        assert_eq!(state.edge(edge).outputs().len(), 2);
        assert_eq!(state.edge(edge).implicit_outs(), 1);
        assert_eq!(state.edge(edge).inputs().len(), 3);
        assert!(!state.node(dd).dyndep_pending);
    }

    #[test]
    fn restat_is_recorded_on_the_edge() {
        let (mut state, disk) = setup();
        disk.create(
            "dd",
            "ninja_dyndep_version = 1\nbuild out: dyndep\n  restat = 1\n",
        );
        let dd = state.lookup_node("dd").unwrap();
        load_dyndeps(&mut state, &disk, dd).unwrap();
        assert!(state.edge_restat(crate::state::EdgeId(0)));
    }

    #[test]
    fn missing_version_is_an_error() {
        let (mut state, disk) = setup();
        disk.create("dd", "build out: dyndep\n");
        let dd = state.lookup_node("dd").unwrap();
        let e = load_dyndeps(&mut state, &disk, dd).unwrap_err();
        assert!(e.to_string().contains("ninja_dyndep_version"), "{e}");
    }

    #[test]
    fn unknown_output_is_an_error() {
        let (mut state, disk) = setup();
        disk.create(
            "dd",
            "ninja_dyndep_version = 1\nbuild nosuch: dyndep\n",
        );
        let dd = state.lookup_node("dd").unwrap();
        let e = load_dyndeps(&mut state, &disk, dd).unwrap_err();
        assert!(
            e.to_string().contains("no build statement exists for 'nosuch'"),
            "{e}"
        );
    }

    #[test]
    fn edge_must_be_mentioned() {
        let (mut state, disk) = setup();
        disk.create("dd", "ninja_dyndep_version = 1\n");
        let dd = state.lookup_node("dd").unwrap();
        let e = load_dyndeps(&mut state, &disk, dd).unwrap_err();
        assert!(e.to_string().contains("not mentioned in its dyndep file"), "{e}");
    }

    #[test]
    fn explicit_inputs_rejected() {
        let (mut state, disk) = setup();
        disk.create(
            "dd",
            "ninja_dyndep_version = 1\nbuild out: dyndep explicit\n",
        );
        let dd = state.lookup_node("dd").unwrap();
        let e = load_dyndeps(&mut state, &disk, dd).unwrap_err();
        assert!(e.to_string().contains("explicit inputs not supported"), "{e}");
    }
}
