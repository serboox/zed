use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, Sender, channel};

use anyhow::{Result, anyhow};
use futures::channel::oneshot;

use crate::{Finding, Severity};

/// How many translation units are kept parsed at once.
///
/// This is the whole memory gate. A translation unit is the entire program the
/// compiler saw -- the file, and every header it reaches, expanded -- and it is
/// what makes a language server for C++ cost hundreds of megabytes: it holds
/// one for every file that has been opened, plus a precompiled preamble for
/// each. Two is the file being read and the one it was switched from, so
/// flipping between a source file and its header is a reparse and not a fresh
/// parse; a third would buy little and cost another whole unit.
///
/// Nothing beyond this is kept: no preamble is precompiled, no completion
/// results are cached, and no preprocessing record is built. Each of those is a
/// second copy of roughly the same information, traded for speed, and this
/// trades the other way.
pub const HELD_AT_MOST: usize = 2;

/// A file to parse, the text to parse instead of what is on disk, and the
/// arguments the compilation database says it is compiled with.
pub struct Request {
    pub file: PathBuf,
    pub text: String,
    pub arguments: Vec<String>,
    pub asked: Asked,
}

/// What a request wants out of the parse.
///
/// Only what was asked for is read back, and both kinds are answered from the
/// same held translation unit, which is what keeps the bound above a bound over
/// the whole feature.
///
/// There is deliberately no third kind asking what could follow the cursor.
/// `clang_codeCompleteAt` answers a null pointer, and prints `crash detected in
/// code completion`, for a unit parsed without
/// `CXTranslationUnit_PrecompiledPreamble` -- and that flag is the front end
/// writing out every header the file reaches, expanded, so that completion can
/// run again without re-reading them. That is a second copy of the translation
/// unit, which is the one thing the bound exists to refuse.
pub enum Asked {
    /// What the front end thinks is wrong with the file.
    WhatIsWrong,
    /// What the entity at this byte offset into the text is.
    WhatIsAt(usize),
    /// Which types the type at one place is related to, read one way round.
    WhichTypesRelateTo { at: Place, direction: Related },
}

/// A place in one of the files that make up a translation unit.
///
/// The file is named because a class is very often not declared in the file
/// being parsed: a base class lives in a header, and the header is part of the
/// same translation unit. The offset is a byte offset into that file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub file: PathBuf,
    pub offset: usize,
}

/// Which way a type hierarchy is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Related {
    /// The classes a class derives from. Its own declaration names them, so
    /// this answer is complete.
    Bases,
    /// The classes that derive from it, as far as this one translation unit
    /// shows. A class deriving from it in a file this unit never reaches is
    /// not here and cannot be: a translation unit is one program the compiler
    /// saw, and it knows nothing of the rest of the project.
    Derived,
}

/// One type the front end named, and where its declaration is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedType {
    /// The name as C++ spells it, template arguments included.
    pub name: String,
    /// The word for what it is, in C++'s own vocabulary rather than the
    /// protocol's.
    pub kind: String,
    /// Where its name is written, which is also all it takes to ask about it
    /// again.
    pub at: Place,
    /// Byte offset in `at.file` just past that name.
    pub end: usize,
}

/// A type and its neighbours in one direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relatives {
    pub subject: NamedType,
    pub related: Vec<NamedType>,
}

/// What the front end knows about one place in the file.
///
/// The type is the point of it. A name index can quote the declaration a name
/// was written with; only the front end can say that
/// `Registry<Key, Value>::keys` returns a `std::vector<std::string>` in the one
/// instantiation the reader is looking at, and templated C++ is where a name on
/// its own says least.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Described {
    /// The name, with the type it has here in front of it, as one line of C++.
    pub declaration: String,
    /// That type on its own: for a call, what it returns; for anything else,
    /// what it is. Every template parameter is already substituted for what it
    /// was instantiated with.
    pub type_of: Option<String>,
    /// The brief of the doc comment on the declaration, where it has one.
    pub comment: Option<String>,
    /// Byte offsets into the text that was parsed, spanning the name asked
    /// about.
    pub start: usize,
    pub end: usize,
}

/// What one parse produced.
///
/// Which of the first two fields carries anything follows from what was
/// [`Asked`]; the other stays empty.
pub struct Parsed {
    pub findings: Vec<Finding>,
    pub described: Option<Described>,
    /// The type asked about and its neighbours, where the place asked about
    /// holds a type at all.
    pub relatives: Option<Relatives>,
    /// What the front end itself says this translation unit costs, in bytes.
    /// Its own accounting, not the process's: the point of the bound above is
    /// that this number is multiplied by at most [`HELD_AT_MOST`].
    pub memory: usize,
    /// How many translation units are held now, so a caller can see the bound
    /// holding rather than take it on trust.
    pub held: usize,
}

/// What identifies a translation unit already parsed. The arguments belong in
/// it: the same file compiled with different defines is a different program,
/// and reparsing the old unit would answer about the wrong one.
#[derive(Debug, PartialEq, Eq, Clone)]
struct Key {
    file: PathBuf,
    arguments: Vec<String>,
}

struct Job {
    request: Request,
    reply: oneshot::Sender<Result<Parsed>>,
}

/// Parses one file and answers what the front end said about it.
///
/// The work happens on one thread that owns `libclang` for the life of the
/// process, because the library is not usable from more than one at a time and
/// its handle cannot be moved between them. Requests queue there; a caller that
/// stops waiting is noticed and its parse is skipped.
///
/// A machine with no `libclang` on it fails here, every time, cheaply. That is
/// the intended failure: the library is looked for when the editor runs and not
/// when it is built, so the editor builds and runs on a machine that has never
/// had LLVM installed, and simply says nothing about C there.
pub async fn ask_libclang(request: Request) -> Result<Parsed> {
    let (reply, answered) = oneshot::channel();
    let jobs = worker().ok_or_else(|| anyhow!("libclang has no thread to run on"))?;
    jobs.send(Job { request, reply })
        .map_err(|_| anyhow!("the libclang thread has stopped"))?;
    answered
        .await
        .map_err(|_| anyhow!("the parse was dropped"))?
}

fn worker() -> Option<&'static Sender<Job>> {
    static WORKER: OnceLock<Option<Sender<Job>>> = OnceLock::new();
    WORKER
        .get_or_init(|| {
            let (send, receive) = channel();
            match std::thread::Builder::new()
                .name("libclang".to_string())
                .spawn(move || serve(&receive))
            {
                Ok(_) => Some(send),
                Err(error) => {
                    log::warn!("no thread for libclang, so no C or C++ diagnostics: {error}");
                    None
                }
            }
        })
        .as_ref()
}

fn serve(jobs: &Receiver<Job>) {
    let clang = match clang::Clang::new() {
        Ok(clang) => clang,
        Err(why) => {
            log::info!(
                "no libclang on this machine, so C and C++ get no diagnostics from one: {why}"
            );
            for job in jobs {
                answer(
                    job.reply,
                    Err(anyhow!("no libclang on this machine: {why}")),
                );
            }
            return;
        }
    };
    // Leaked on purpose, both of them. This thread runs for as long as the
    // editor does, so neither is ever dropped in practice -- and dropping the
    // first would unload the shared library only to load it again. Making them
    // `'static` is also what lets a parsed unit be kept in the list below:
    // borrowed from a local, a unit could not outlive the statement that made
    // it.
    let clang: &'static clang::Clang = Box::leak(Box::new(clang));
    let exclude_declarations_from_precompiled_headers = true;
    let print_diagnostics_to_the_terminal = false;
    let index: &'static clang::Index<'static> = Box::leak(Box::new(clang::Index::new(
        clang,
        exclude_declarations_from_precompiled_headers,
        print_diagnostics_to_the_terminal,
    )));

    let mut held: Vec<(Key, clang::TranslationUnit<'static>)> = Vec::new();
    for job in jobs {
        // A reader who has saved again, or closed the file, is not owed the
        // answer to the question they have stopped asking -- and a parse is the
        // most expensive thing this thread does.
        if job.reply.is_canceled() {
            continue;
        }
        let parsed = without_taking_the_thread_down(index, &mut held, &job.request);
        answer(job.reply, parsed);
    }
}

/// One parse, with a panic in it turned into a failed request.
///
/// The bindings assert on a `libclang` entry point that answers a null pointer,
/// and one such assert on this thread would end it -- and with it every C and
/// C++ answer for the rest of the session, since this is the only thread the
/// library is usable from. A wrong answer to one request is the smaller
/// failure. The held units are handed across the boundary deliberately: a
/// panicking parse never reached the line that adds one, so the list is either
/// untouched or one unit shorter than it was.
fn without_taking_the_thread_down(
    index: &'static clang::Index<'static>,
    held: &mut Vec<(Key, clang::TranslationUnit<'static>)>,
    request: &Request,
) -> Result<Parsed> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        parse_one(index, held, request)
    }))
    .unwrap_or_else(|_| Err(anyhow!("libclang gave up on {}", request.file.display())))
}

/// The asker gives up by dropping its end of the channel, which is ordinary and
/// not a failure.
fn answer(reply: oneshot::Sender<Result<Parsed>>, parsed: Result<Parsed>) {
    if reply.send(parsed).is_err() {
        log::debug!("nobody was left waiting for a libclang parse");
    }
}

fn parse_one(
    index: &'static clang::Index<'static>,
    held: &mut Vec<(Key, clang::TranslationUnit<'static>)>,
    request: &Request,
) -> Result<Parsed> {
    let key = Key {
        file: request.file.clone(),
        arguments: request.arguments.clone(),
    };
    let unsaved = [clang::Unsaved::new(&request.file, &request.text)];

    // Reparsing a unit the front end already has costs a fraction of building
    // one, which is the reason to keep any at all.
    let already = reuse(held.iter().map(|(key, _)| key), &key);
    let unit = match already.map(|at| held.remove(at)) {
        Some((_, unit)) => match unit.reparse(&unsaved) {
            Ok(unit) => unit,
            // A reparse can fail on a file changed past what the front end can
            // patch up, and the answer then is a fresh parse rather than none.
            Err(error) => {
                log::debug!("reparsing {}: {error}", request.file.display());
                fresh(index, request, &unsaved)?
            }
        },
        None => fresh(index, request, &unsaved)?,
    };

    let mut findings = Vec::new();
    let mut described = None;
    let mut relatives = None;
    match &request.asked {
        Asked::WhatIsWrong => findings = findings_of(&unit, &request.file),
        Asked::WhatIsAt(offset) => {
            described = description_at(&unit, &request.file, &request.text, *offset);
        }
        Asked::WhichTypesRelateTo { at, direction } => {
            relatives = relatives_at(&unit, at, *direction);
        }
    }
    let memory = unit.get_memory_usage().values().sum();

    held.insert(0, (key, unit));
    // Everything past the bound is dropped here, and with it the memory the
    // front end was holding for it.
    held.truncate(HELD_AT_MOST);

    Ok(Parsed {
        findings,
        described,
        relatives,
        memory,
        held: held.len(),
    })
}

/// What the front end knows about the byte offset asked about.
///
/// The type is read off the occurrence under the cursor rather than off the
/// declaration it refers to. Inside the template, `keys` returns
/// `std::vector<Key>`; it is the use of it through a
/// `Registry<std::string, ...>` that knows the answer is
/// `std::vector<std::string>`, and that answer is the whole reason to ask a
/// compiler rather than an index.
fn description_at<'a>(
    unit: &'a clang::TranslationUnit<'a>,
    about: &Path,
    text: &str,
    offset: usize,
) -> Option<Described> {
    let file = unit.get_file(about)?;
    let at = file.get_offset_location(u32::try_from(offset).ok()?);
    let entity = at.get_entity()?;
    // Everything the parse did not understand comes back as the unit itself,
    // and the unit's name is the file's path.
    if entity.get_kind() == clang::EntityKind::TranslationUnit {
        return None;
    }
    let declared = entity.get_reference().unwrap_or(entity);
    let name = declared
        .get_display_name()
        .or_else(|| declared.get_name())
        .filter(|name| !name.is_empty())?;

    // A member function named through an instance has a type the front end will
    // only spell as `<bound member function type>`, and the same goes for a
    // dependent or overloaded one. Where it says that, the declaration it
    // refers to is where the real signature is -- already instantiated, because
    // the lookup happened in the instantiated class and not in the template.
    let here = printable(entity.get_type()).or_else(|| printable(declared.get_type()));
    let type_of = match &here {
        Some(here)
            if matches!(
                here.get_kind(),
                clang::TypeKind::FunctionPrototype | clang::TypeKind::FunctionNoPrototype
            ) =>
        {
            here.get_result_type()
                .map(|result| result.get_display_name())
        }
        Some(here) => Some(here.get_display_name()),
        None => None,
    };
    let declaration = match &type_of {
        Some(type_of) => format!("{type_of} {name}"),
        None => name,
    };

    let (start, end) =
        name_span(&entity, about, offset).unwrap_or_else(|| word_around(text, offset));
    Some(Described {
        declaration,
        type_of,
        comment: declared
            .get_comment_brief()
            .map(|brief| brief.trim().to_string())
            .filter(|brief| !brief.is_empty()),
        start,
        end,
    })
}

/// A type only where the front end has a name for it.
///
/// Where it does not, it answers with a placeholder in angle brackets -- and a
/// card reading `<bound member function type> keys` tells the reader less than
/// no card at all.
fn printable<'a>(kind: Option<clang::Type<'a>>) -> Option<clang::Type<'a>> {
    kind.filter(|kind| !kind.get_display_name().starts_with('<'))
}

/// The span of just the name at the cursor, as the front end spells it out.
///
/// An expression's own range is wider than its name -- `registry.keys` for the
/// `keys` in it -- and underlining the whole of that says the card is about
/// something it is not.
fn name_span<'a>(
    entity: &clang::Entity<'a>,
    about: &Path,
    offset: usize,
) -> Option<(usize, usize)> {
    entity
        .get_name_ranges()
        .iter()
        .map(|range| {
            (
                range.get_start().get_file_location(),
                range.get_end().get_file_location(),
            )
        })
        .find(|(start, end)| {
            start.file.map(|file| file.get_path()).as_deref() == Some(about)
                && (start.offset as usize) <= offset
                && offset <= (end.offset as usize)
        })
        .map(|(start, end)| (start.offset as usize, end.offset as usize))
}

/// The identifier the offset falls in, read off the text.
///
/// The fallback for a place the front end gives no name range for, so that the
/// card still underlines a word rather than an empty point.
fn word_around(text: &str, offset: usize) -> (usize, usize) {
    let offset = at_a_boundary(text, offset);
    let is_name = |character: char| character.is_alphanumeric() || character == '_';
    let start = text[..offset]
        .char_indices()
        .rev()
        .take_while(|(_, character)| is_name(*character))
        .last()
        .map_or(offset, |(at, _)| at);
    let end = text[offset..]
        .char_indices()
        .take_while(|(_, character)| is_name(*character))
        .last()
        .map_or(offset, |(at, character)| offset + at + character.len_utf8());
    (start, end)
}

/// The nearest offset at or before the given one that a `str` can be split at.
///
/// The text is the buffer's and the offset came from it, so this only matters
/// where they have drifted apart -- but slicing between the bytes of one
/// character panics, and taking the editor down over a stale offset is not an
/// option.
fn at_a_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// The type at one place, and its neighbours in one direction.
///
/// `None` where that place holds no type at all -- a statement, a call, a
/// keyword, a file the unit never reached. That is the difference between "no
/// supertypes" and "not a question about a type", and the two must not read
/// the same.
fn relatives_at<'a>(
    unit: &'a clang::TranslationUnit<'a>,
    at: &Place,
    direction: Related,
) -> Option<Relatives> {
    let subject = class_at(unit, at)?;
    let related = match direction {
        Related::Bases => bases_of(&subject),
        Related::Derived => classes_derived_from(unit, &subject),
    };
    let mut named_related: Vec<NamedType> = Vec::new();
    for one in related {
        let Some(named) = named(&one) else {
            continue;
        };
        // A class the unit reaches twice -- declared in a header, derived from
        // in two files it includes -- is one class, and its place is what says
        // so.
        if !named_related.iter().any(|already| already.at == named.at) {
            named_related.push(named);
        }
    }
    Some(Relatives {
        subject: named(&subject)?,
        related: named_related,
    })
}

/// The class the given place is about.
///
/// A cursor written on the class's own name is the easy case. A cursor on
/// something that merely *has* that type -- a variable, a field, a base in a
/// base clause -- answers about the type it has, which is what a reader asking
/// for a hierarchy from a declaration line means.
fn class_at<'a>(unit: &'a clang::TranslationUnit<'a>, at: &Place) -> Option<clang::Entity<'a>> {
    let file = unit.get_file(&at.file)?;
    let entity = file
        .get_offset_location(u32::try_from(at.offset).ok()?)
        .get_entity()?;
    // Everything the parse did not understand comes back as the unit itself.
    if entity.get_kind() == clang::EntityKind::TranslationUnit {
        return None;
    }
    let declared = entity.get_reference().unwrap_or(entity);
    if is_a_class(declared.get_kind()) {
        return Some(declared.get_canonical_entity());
    }
    let through = declared.get_type()?.get_declaration()?;
    is_a_class(through.get_kind()).then(|| through.get_canonical_entity())
}

/// The classes named in a class's own base clause, in the order it names them.
///
/// Read off the definition rather than off whichever declaration was asked
/// about: a forward declaration names no bases, and answering "none" from one
/// would be a wrong answer rather than a missing one.
fn bases_of<'a>(subject: &clang::Entity<'a>) -> Vec<clang::Entity<'a>> {
    let definition = subject.get_definition().unwrap_or(*subject);
    definition
        .get_children()
        .into_iter()
        .filter(|child| child.get_kind() == clang::EntityKind::BaseSpecifier)
        .filter_map(|base| {
            base.get_type()
                .and_then(|of| of.get_declaration())
                .or_else(|| base.get_reference())
        })
        .map(|base| base.get_canonical_entity())
        .collect()
}

/// Every class in this translation unit whose base clause names the subject.
///
/// One direction of the same edge, walked the only way a translation unit
/// allows: over everything it holds. What it holds is the file that was parsed
/// and every header that file reaches, and nothing else -- so a class derived
/// from in a file nobody included is genuinely absent, not overlooked.
fn classes_derived_from<'a>(
    unit: &'a clang::TranslationUnit<'a>,
    subject: &clang::Entity<'a>,
) -> Vec<clang::Entity<'a>> {
    let Some(wanted) = subject.get_canonical_entity().get_usr() else {
        return Vec::new();
    };
    let mut derived = Vec::new();
    unit.get_entity().visit_children(|entity, _| {
        if is_a_class(entity.get_kind())
            && entity.is_definition()
            && bases_of(&entity)
                .iter()
                .any(|base| base.get_usr().as_ref() == Some(&wanted))
        {
            derived.push(entity.get_canonical_entity());
        }
        clang::EntityVisitResult::Recurse
    });
    derived
}

fn is_a_class(kind: clang::EntityKind) -> bool {
    matches!(
        kind,
        clang::EntityKind::ClassDecl
            | clang::EntityKind::StructDecl
            | clang::EntityKind::UnionDecl
            | clang::EntityKind::ClassTemplate
            | clang::EntityKind::ClassTemplatePartialSpecialization
    )
}

/// A class as a row can show it: its name, the word for what it is, and where
/// its name is written.
fn named(entity: &clang::Entity<'_>) -> Option<NamedType> {
    let declaration = entity.get_definition().unwrap_or(*entity);
    let name = declaration
        .get_display_name()
        .or_else(|| declaration.get_name())
        .filter(|name| !name.is_empty())?;
    let at = declaration.get_location()?.get_file_location();
    let file = at.file?.get_path();
    let start = at.offset as usize;
    // The name range where the front end gives one, and the identifier's own
    // length otherwise, so a row underlines the name rather than a point.
    let end = declaration
        .get_name_ranges()
        .first()
        .map(|range| range.get_end().get_file_location().offset as usize)
        .filter(|end| *end > start)
        .unwrap_or_else(|| start + declaration.get_name().unwrap_or_default().len());
    Some(NamedType {
        name,
        kind: word_for(declaration.get_kind()).to_string(),
        at: Place {
            file,
            offset: start,
        },
        end,
    })
}

/// What a reader would call it, in C++'s own words. A `struct` is not a
/// `class` in the source even where the language treats them alike, and a row
/// that renamed it would send the reader looking for a keyword that is not
/// there.
fn word_for(kind: clang::EntityKind) -> &'static str {
    match kind {
        clang::EntityKind::ClassDecl => "class",
        clang::EntityKind::StructDecl => "struct",
        clang::EntityKind::UnionDecl => "union",
        clang::EntityKind::ClassTemplate => "class template",
        clang::EntityKind::ClassTemplatePartialSpecialization => "class template specialization",
        _ => "type",
    }
}

fn fresh(
    index: &'static clang::Index<'static>,
    request: &Request,
    unsaved: &[clang::Unsaved],
) -> Result<clang::TranslationUnit<'static>> {
    index
        .parser(&request.file)
        .arguments(&request.arguments)
        .unsaved(unsaved)
        // Carry on past an error rather than stopping at the first one: a
        // reader wants the list, and a missing header at the top of a file
        // would otherwise be the only thing ever reported.
        .keep_going(true)
        // A warning in a header is about the header, and there are thousands of
        // them in a system one. Errors from headers still arrive.
        .ignore_non_errors_from_included_files(true)
        .parse()
        .map_err(|error| anyhow!("parsing {}: {error}", request.file.display()))
}

/// Which held unit answers this request, if any.
fn reuse<'k>(held: impl Iterator<Item = &'k Key>, wanted: &Key) -> Option<usize> {
    held.enumerate()
        .find(|(_, key)| *key == wanted)
        .map(|(at, _)| at)
}

/// What the front end said about the file it was asked about.
///
/// Findings in other files are dropped. This source is about the file the
/// reader has open; a diagnostic belongs on the file it is in, and putting a
/// header's error on the line that included it would say the wrong thing about
/// the wrong line. A header opened on its own is parsed on its own and reports
/// there.
///
/// The lifetime is written out rather than elided: a unit is kept across
/// requests as a `'static` one, and reading it has to borrow it for less than
/// that or it could never be moved again.
fn findings_of<'a>(unit: &'a clang::TranslationUnit<'a>, about: &Path) -> Vec<Finding> {
    unit.get_diagnostics()
        .iter()
        .filter_map(|diagnostic| finding_of(diagnostic, about))
        .collect()
}

fn finding_of<'a>(diagnostic: &clang::diagnostic::Diagnostic<'a>, about: &Path) -> Option<Finding> {
    let severity = match diagnostic.get_severity() {
        clang::diagnostic::Severity::Ignored => return None,
        clang::diagnostic::Severity::Note => Severity::Note,
        clang::diagnostic::Severity::Warning => Severity::Warning,
        clang::diagnostic::Severity::Error | clang::diagnostic::Severity::Fatal => Severity::Error,
    };
    let at = diagnostic.get_location().get_file_location();
    if at.file?.get_path() != about {
        return None;
    }

    // The point the front end blamed, widened to the range it highlighted where
    // it gave one. A range is what marks the whole of a bad expression rather
    // than its first character.
    let (start, end) = diagnostic
        .get_ranges()
        .iter()
        .map(|range| {
            (
                range.get_start().get_file_location(),
                range.get_end().get_file_location(),
            )
        })
        .find(|(start, end)| {
            start.file.map(|file| file.get_path()).as_deref() == Some(about)
                && end.offset >= start.offset
        })
        .map_or((at.offset, at.offset), |(start, end)| {
            (start.offset, end.offset)
        });

    Some(Finding {
        severity,
        message: whole_of_what_it_said(diagnostic),
        start: start as usize,
        end: end as usize,
    })
}

/// A diagnostic and the notes hung off it, as one message.
///
/// The note is often where the answer is -- which overload was tried, where the
/// conflicting declaration is -- and a note shown on its own line miles away
/// from the error it explains is a note nobody reads.
fn whole_of_what_it_said(diagnostic: &clang::diagnostic::Diagnostic<'_>) -> String {
    let mut said = diagnostic.get_text();
    for note in diagnostic.get_children() {
        said.push_str("\nnote: ");
        said.push_str(&note.get_text());
    }
    said
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn key(file: &str, arguments: &[&str]) -> Key {
        Key {
            file: PathBuf::from(file),
            arguments: arguments
                .iter()
                .map(|argument| (*argument).to_string())
                .collect(),
        }
    }

    /// The same file compiled with different defines is a different program,
    /// and reparsing the unit built for the other one would answer about code
    /// the reader is not looking at.
    #[test]
    fn a_unit_is_reused_only_for_the_same_file_and_the_same_arguments() {
        let held = [key("/p/a.cpp", &["-DA"]), key("/p/b.cpp", &["-DA"])];

        assert_eq!(reuse(held.iter(), &key("/p/a.cpp", &["-DA"])), Some(0));
        assert_eq!(reuse(held.iter(), &key("/p/b.cpp", &["-DA"])), Some(1));
        assert_eq!(reuse(held.iter(), &key("/p/a.cpp", &["-DB"])), None);
        assert_eq!(reuse(held.iter(), &key("/p/c.cpp", &["-DA"])), None);
    }

    /// The bound is the point of the whole thing: a translation unit is what a
    /// C++ language server spends its hundreds of megabytes on, and this holds
    /// a fixed number of them however many files are opened.
    #[test]
    fn no_more_than_the_bound_are_ever_held() {
        let mut held: Vec<Key> = Vec::new();
        for file in ["a", "b", "c", "d", "e", "f"] {
            held.insert(0, key(&format!("/p/{file}.cpp"), &[]));
            held.truncate(HELD_AT_MOST);
            assert!(held.len() <= HELD_AT_MOST, "{} held", held.len());
        }
        assert_eq!(
            held,
            vec![key("/p/f.cpp", &[]), key("/p/e.cpp", &[])],
            "the newest, and the one before it"
        );
    }

    /// What translation units really cost, on this machine, with this
    /// `libclang`, and whether the bound above actually holds them down.
    ///
    /// Not an assertion about the numbers: they depend on the standard library
    /// the machine has, and a test that failed on a bigger `<vector>` would say
    /// nothing useful. It does assert the bound, because that is the claim.
    /// Run it deliberately:
    ///
    /// `cargo test -p clang_diagnostics --lib -- --ignored --nocapture`
    #[test]
    #[ignore = "reports a measurement, and needs libclang on the machine"]
    fn what_translation_units_cost_and_how_many_are_held() {
        let directory = std::env::temp_dir().join("clang_diagnostics_measurement");
        std::fs::create_dir_all(&directory).expect("a directory to measure in");

        // More files than the bound, so that the bound is what stops the list
        // growing rather than there being nothing else to hold.
        let files: Vec<PathBuf> = (0..HELD_AT_MOST + 2)
            .map(|which| {
                let file = directory.join(format!("measured_{which}.cpp"));
                std::fs::write(&file, MEASURED).expect("a file to measure");
                file
            })
            .collect();

        let hovered = MEASURED
            .rfind("keys")
            .expect("the call to measure a hover on");
        let declared = MEASURED
            .find("Named {")
            .expect("the class to measure a hierarchy question on");

        // Twice around, so the second lap also exercises the reparse of a unit
        // that is still held and the fresh parse of one that has been evicted.
        // Both request kinds go through the same held units, which is what
        // makes the bound below a bound over the whole feature and not over
        // diagnostics alone.
        for lap in 1..=2 {
            for file in &files {
                let questions = [
                    Asked::WhatIsWrong,
                    Asked::WhatIsAt(hovered),
                    Asked::WhichTypesRelateTo {
                        at: Place {
                            file: file.clone(),
                            offset: declared,
                        },
                        direction: Related::Derived,
                    },
                ];
                for asked in questions {
                    let what = match asked {
                        Asked::WhatIsWrong => "wrong",
                        Asked::WhatIsAt(_) => "at",
                        Asked::WhichTypesRelateTo { .. } => "types",
                    };
                    let started = std::time::Instant::now();
                    let parsed = futures::executor::block_on(ask_libclang(Request {
                        file: file.clone(),
                        text: MEASURED.to_string(),
                        arguments: vec!["-std=c++20".to_string()],
                        asked,
                    }))
                    .expect("a parse");
                    assert!(
                        parsed.held <= HELD_AT_MOST,
                        "{} units held, bound is {HELD_AT_MOST}",
                        parsed.held,
                    );
                    println!(
                        "lap {lap} {} {what}: {:.0} ms, {:.1} MB in the unit, {} held, \
                         {} findings, {}, {}, {}",
                        file.file_name().unwrap_or_default().to_string_lossy(),
                        started.elapsed().as_secs_f64() * 1_000.0,
                        parsed.memory as f64 / 1_048_576.0,
                        parsed.held,
                        parsed.findings.len(),
                        parsed
                            .described
                            .map_or("nothing described".to_string(), |said| said.declaration),
                        parsed
                            .relatives
                            .map_or("no type asked about".to_string(), |about| {
                                format!(
                                    "{} with {} relative(s)",
                                    about.subject.name,
                                    about.related.len()
                                )
                            }),
                        resident(),
                    );
                }
            }
        }

        std::fs::remove_dir_all(&directory).expect("the directory to go again");
    }

    fn resident() -> String {
        std::fs::read_to_string("/proc/self/status")
            .unwrap_or_default()
            .lines()
            .filter(|line| line.starts_with("VmRSS") || line.starts_with("VmHWM"))
            .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// A file with enough of the standard library in it to be a fair
    /// measurement, and one real mistake so the findings are not empty.
    const MEASURED: &str = r#"
#include <algorithm>
#include <map>
#include <memory>
#include <string>
#include <unordered_map>
#include <vector>

template <typename Key, typename Value>
class Registry {
  public:
    void add(Key key, Value value) { entries_.emplace(std::move(key), std::move(value)); }

    std::vector<Key> keys() const {
        std::vector<Key> found;
        found.reserve(entries_.size());
        for (const auto &entry : entries_) {
            found.push_back(entry.first);
        }
        std::sort(found.begin(), found.end());
        return found;
    }

  private:
    std::map<Key, Value> entries_;
};

class Named {
  public:
    virtual ~Named() = default;
};

class Person : public Named {};

int main() {
    Registry<std::string, std::unique_ptr<int>> registry;
    registry.add("one", std::make_unique<int>(1));
    auto names = registry.keys();
    int missing = registry.nothing_of_the_sort();
    return missing + static_cast<int>(names.size());
}
"#;
}
