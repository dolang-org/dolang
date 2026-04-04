use std::{
    borrow::Cow,
    collections::{HashMap, hash_map::Entry},
    mem,
    ops::ControlFlow,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use tokio::sync::{Mutex, oneshot};
use tower_lsp_server::{
    Client, ClientSocket, LanguageServer, LspService,
    jsonrpc::{self, Result},
    ls_types::*,
};

use dolang_compile::{Compiler, Context, Origin, Token, diag};

use crate::vm::{self, Cmd};

const TOKEN_MODIFIERS: &[SemanticTokenModifier] = &[SemanticTokenModifier::DEFAULT_LIBRARY];

const LEGEND_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::ENUM_MEMBER,
    SemanticTokenType::OPERATOR,
    SemanticTokenType::STRING,
    SemanticTokenType::PROPERTY,
    SemanticTokenType::FUNCTION,
    SemanticTokenType::KEYWORD,
    SemanticTokenType::NUMBER,
    SemanticTokenType::PARAMETER,
    SemanticTokenType::VARIABLE,
    SemanticTokenType::NAMESPACE,
    SemanticTokenType::COMMENT,
    SemanticTokenType::CLASS,
];

const TT_CONSTANT: u32 = 0;
const TT_OPERATOR: u32 = 1;
const TT_STRING: u32 = 2;
const TT_PROPERTY: u32 = 3;
const TT_FUNCTION: u32 = 4;
const TT_KEYWORD: u32 = 5;
const TT_NUMBER: u32 = 6;
const TT_PARAMETER: u32 = 7;
const TT_VARIABLE: u32 = 8;
const TT_NAMESPACE: u32 = 9;
const TT_COMMENT: u32 = 10;
const TT_CLASS: u32 = 11;

const MOD_PRELUDE: u32 = 1;

fn classify_token(token: Token, origin: Option<&Origin>, context: Context) -> (u32, u32) {
    match token {
        Token::Comment => (TT_COMMENT, 0),
        Token::Constant => (TT_CONSTANT, 0),
        Token::Delim => (TT_OPERATOR, 0),
        Token::Escape => (TT_STRING, 0),
        Token::Field => match context {
            Context::Call => (TT_FUNCTION, 0),
            Context::None => (TT_PROPERTY, 0),
        },
        Token::Key => (TT_PROPERTY, 0),
        Token::ModuleName => (TT_NAMESPACE, 0),
        Token::ModuleItem => (TT_PROPERTY, 0),
        Token::Keyword => (TT_KEYWORD, 0),
        Token::Literal => (TT_STRING, 0),
        Token::Number => (TT_NUMBER, 0),
        Token::Operator => (TT_OPERATOR, 0),
        Token::StringDelim => (TT_STRING, 0),
        Token::Variable => match (context, origin) {
            (_, Some(Origin::Class { .. })) => (TT_CLASS, 0),
            (Context::Call, Some(Origin::PreludeItem { .. })) => (TT_FUNCTION, MOD_PRELUDE),
            (Context::Call, Some(Origin::PreludeModule { .. })) => (TT_FUNCTION, MOD_PRELUDE),
            (Context::Call, _) => (TT_FUNCTION, 0),
            (Context::None, Some(Origin::Param { .. } | Origin::SelfParam { .. })) => {
                (TT_PARAMETER, 0)
            }
            (Context::None, Some(Origin::Def { .. })) => (TT_FUNCTION, 0),
            (Context::None, Some(Origin::PreludeItem { .. })) => (TT_VARIABLE, MOD_PRELUDE),
            (Context::None, Some(Origin::PreludeModule { .. })) => (TT_NAMESPACE, MOD_PRELUDE),
            (Context::None, Some(Origin::ImportModule { .. })) => (TT_NAMESPACE, 0),
            (Context::None, _) => (TT_VARIABLE, 0),
        },
        Token::Sigil => (TT_VARIABLE, 0),
    }
}

#[derive(Debug, Clone)]
struct Patch {
    diagnostic_range: Range,
    diagnostic_severity: DiagnosticSeverity,
    diagnostic_message: String,
    patch_range: Range,
    replacement: String,
    title: String,
}

#[derive(Debug, Default)]
struct Document {
    content: String,
    tokens: Vec<SemanticToken>,
    defs: Vec<(Range, Range)>,
    patches: Vec<Patch>,
}

#[derive(Debug)]
pub(crate) enum Import {
    Module(String),
    ModuleAs(String, String),
    Item(String, String),
    ItemAs(String, String, String),
}

#[derive(Debug)]
pub(crate) struct Settings {
    pub(crate) prelude: Vec<Import>,
}

#[derive(Debug, Default)]
struct Config {
    root: Option<PathBuf>,
    workspaces: Vec<PathBuf>,
    settings: HashMap<PathBuf, Arc<Settings>>,
}

#[derive(Debug)]
pub(crate) struct Backend {
    client: Client,
    documents: Mutex<HashMap<Uri, Arc<Mutex<Document>>>>,
    config: Mutex<Config>,
    position_encoding: RwLock<PositionEncodingKind>,
    vm: vm::Vm,
}

#[derive(Debug)]
struct DocumentIndex<'a> {
    content: &'a str,
    line_starts: Vec<usize>,
    position_encoding: PositionEncodingKind,
}

impl<'a> DocumentIndex<'a> {
    fn new(content: &'a str, position_encoding: PositionEncodingKind) -> Self {
        let mut line_starts = vec![0];
        for (offset, ch) in content.char_indices() {
            if ch == '\n' {
                line_starts.push(offset + 1);
            }
        }
        Self {
            content,
            line_starts,
            position_encoding,
        }
    }

    fn line_start(&self, offset: usize) -> (u32, usize) {
        debug_assert!(offset <= self.content.len());
        debug_assert!(self.content.is_char_boundary(offset));
        let line = self.line_starts.partition_point(|&start| start <= offset) - 1;
        (line as u32, self.line_starts[line])
    }

    fn position_from_offset(&self, offset: usize) -> Position {
        let (line, line_start) = self.line_start(offset);
        let character = if self.position_encoding == PositionEncodingKind::UTF8 {
            (offset - line_start) as u32
        } else if self.position_encoding == PositionEncodingKind::UTF16 {
            self.content[line_start..offset].encode_utf16().count() as u32
        } else {
            unreachable!("unsupported position encoding")
        };
        Position::new(line, character)
    }

    fn range_from_offsets(&self, start: usize, end: usize) -> Range {
        Range::new(
            self.position_from_offset(start),
            self.position_from_offset(end),
        )
    }

    fn range_from_span(&self, span: &diag::Span) -> Range {
        self.range_from_offsets(span.start().byte_offset(), span.end().byte_offset())
    }

    fn token_length_from_offsets(&self, start: usize, end: usize) -> u32 {
        debug_assert!(self.content.is_char_boundary(start));
        debug_assert!(self.content.is_char_boundary(end));
        if self.position_encoding == PositionEncodingKind::UTF8 {
            (end - start) as u32
        } else if self.position_encoding == PositionEncodingKind::UTF16 {
            self.content[start..end].encode_utf16().count() as u32
        } else {
            unreachable!("unsupported position encoding")
        }
    }

    fn token_length(&self, span: &diag::Span) -> u32 {
        self.token_length_from_offsets(span.start().byte_offset(), span.end().byte_offset())
    }
}

impl Backend {
    pub(crate) fn new(client: Client) -> Self {
        Self {
            client,
            documents: Default::default(),
            config: Default::default(),
            position_encoding: RwLock::new(PositionEncodingKind::UTF16),
            vm: vm::Vm::new(),
        }
    }

    async fn find_settings(&self, path: &Path) -> Option<Arc<Settings>> {
        let guard = self.config.lock().await;
        let mut cur = path.parent();
        let mut config_file = None;

        while let Some(dir) = cur {
            cur = dir.parent();
            if let Some(settings) = guard.settings.get(dir) {
                return Some(settings.clone());
            }
            let candidate = dir.join(".dolang-lsp.dol");
            if candidate.is_file() {
                config_file = Some(candidate);
                break;
            }
        }
        mem::drop(guard);

        if let Some(config) = config_file {
            let (send, recv) = oneshot::channel();
            if let Err(e) = self.vm.send(Cmd::ReadSettings(config.clone(), send)).await {
                log::error!("failed to load configuration {}: {}", config.display(), e);
                return None;
            }
            let settings = match recv.await {
                Err(e) => {
                    log::error!("failed to load configuration {}: {}", config.display(), e);
                    return None;
                }
                Ok(Err(e)) => {
                    log::error!("failed to load configuration {}: {}", config.display(), e);
                    return None;
                }
                Ok(Ok(settings)) => settings,
            };
            let settings = Arc::new(settings);
            let mut guard = self.config.lock().await;
            guard.settings.insert(path.to_owned(), settings.clone());
            return Some(settings);
        }
        Some(Arc::new(Settings {
            prelude: vec![
                Import::Item("proc".into(), "sub".into()),
                Import::Item("sys".into(), "cd".into()),
                Import::Item("sys".into(), "echo".into()),
                Import::Item("sys".into(), "env".into()),
                Import::Item("sys".into(), "exit".into()),
                Import::Item("sys".into(), "print".into()),
                Import::ModuleAs("proc.run".into(), "run".into()),
            ],
        }))
    }

    fn choose_position_encoding(params: &InitializeParams) -> PositionEncodingKind {
        let offered = params
            .capabilities
            .general
            .as_ref()
            .and_then(|general| general.position_encodings.as_ref());
        if offered.is_some_and(|encodings| encodings.contains(&PositionEncodingKind::UTF8)) {
            PositionEncodingKind::UTF8
        } else {
            PositionEncodingKind::UTF16
        }
    }

    fn position_encoding(&self) -> PositionEncodingKind {
        self.position_encoding
            .read()
            .expect("position encoding lock poisoned")
            .clone()
    }

    async fn on_change(&self, params: TextDocumentItem) {
        let TextDocumentItem {
            uri, text, version, ..
        } = params;

        let document = {
            let mut guard = self.documents.lock().await;
            match guard.entry(uri.clone()) {
                Entry::Occupied(entry) => entry.get().clone(),
                Entry::Vacant(entry) => {
                    let doc = Arc::new(Mutex::new(Default::default()));
                    entry.insert(doc.clone());
                    doc
                }
            }
        };
        let mut diags = Vec::new();
        let Some(path) = uri_to_file_path(&uri) else {
            return;
        };
        {
            let settings = self.find_settings(&path).await;

            let mut guard = document.lock().await;
            guard.content = text;
            let mut tokens = Vec::new();
            let mut defs = Vec::new();
            let mut patches = Vec::new();

            let content = guard.content.as_str();
            let index = DocumentIndex::new(content, self.position_encoding());
            let mut compiler = Compiler::new(&path, content.as_bytes());
            if let Some(settings) = settings {
                let mut prelude = compiler.prelude();
                for import in settings.prelude.iter() {
                    match import {
                        Import::Module(module) => {
                            prelude = prelude.import_module(module.clone());
                        }
                        Import::Item(module, item) => {
                            let items = prelude.import_items(module.clone());
                            prelude = items.item(item.clone()).commit();
                        }
                        Import::ModuleAs(module, bind) => {
                            prelude = prelude.import_module_with_name(module.clone(), bind.clone());
                        }
                        Import::ItemAs(module, item, bind) => {
                            let items = prelude.import_items(module.clone());
                            prelude = items.item_with_name(item.clone(), bind.clone()).commit();
                        }
                    }
                }
            }
            compiler
                .analyze(
                    &mut |diag: diag::Diag| -> ControlFlow<()> {
                        let mut out = Diagnostic::new_simple(
                            index.range_from_span(&diag.span()),
                            diag.message().to_string(),
                        );
                        out.severity = Some(match diag.severity() {
                            diag::Severity::Error => DiagnosticSeverity::ERROR,
                            diag::Severity::Warning => DiagnosticSeverity::WARNING,
                            _ => DiagnosticSeverity::INFORMATION,
                        });
                        let mut related = Vec::new();
                        for ann in diag.annotations() {
                            related.push(DiagnosticRelatedInformation {
                                location: Location::new(
                                    uri.clone(),
                                    index.range_from_span(&ann.span()),
                                ),
                                message: ann.message().to_string(),
                            });
                        }
                        out.related_information = Some(related);
                        diags.push(out);
                        for note in diag.notes() {
                            let mut out = Diagnostic::new_simple(
                                index.range_from_span(&diag.span()),
                                note.message().to_string(),
                            );
                            out.severity = Some(match note.kind() {
                                diag::NoteKind::Help => DiagnosticSeverity::HINT,
                                _ => DiagnosticSeverity::INFORMATION,
                            });
                            diags.push(out);
                        }

                        let diagnostic_range = index.range_from_span(&diag.span());
                        let diagnostic_message = diag.message().to_string();
                        let diagnostic_severity = match diag.severity() {
                            diag::Severity::Error => DiagnosticSeverity::ERROR,
                            diag::Severity::Warning => DiagnosticSeverity::WARNING,
                            _ => DiagnosticSeverity::INFORMATION,
                        };

                        for patch in diag.patches() {
                            patches.push(Patch {
                                diagnostic_range,
                                diagnostic_severity,
                                diagnostic_message: diagnostic_message.clone(),
                                patch_range: index.range_from_span(&patch.span()),
                                replacement: patch.sub().to_string(),
                                title: patch.message().to_string(),
                            });
                        }

                        ControlFlow::Continue(())
                    },
                    &mut |leaf, span: diag::Span, origin: Option<Origin>, context: Context| {
                        if span.start().byte_offset() != span.end().byte_offset()
                            && !matches!(leaf, Token::Delim)
                        {
                            let (token_type, modifiers) =
                                classify_token(leaf, origin.as_ref(), context);
                            if let Some(origin) = origin
                                && let Some(def) = match origin {
                                    Origin::ImportItem { name, .. } => Some(name),
                                    Origin::ImportModule { name, .. } => Some(name),
                                    Origin::PreludeModule { .. } => None,
                                    Origin::PreludeItem { .. } => None,
                                    Origin::Class { span } => Some(span),
                                    Origin::Def { span, .. } => Some(span),
                                    Origin::Bind { span, .. } => Some(span),
                                    Origin::Param { span } | Origin::SelfParam { span } => {
                                        Some(span)
                                    }
                                }
                            {
                                defs.push((
                                    index.range_from_span(&span),
                                    index.range_from_span(&def),
                                ))
                            }
                            tokens.push((token_type, modifiers, span));
                        }
                        ControlFlow::Continue(())
                    },
                )
                .unwrap();

            let mut pre_line = 0;
            let mut pre_start = 0;

            tokens.sort_by_key(|(_, _, range)| range.start().byte_offset());
            defs.sort_by_key(|(range, _)| range.start);

            guard.tokens = tokens
                .into_iter()
                .map(|(token_type, modifiers, range)| {
                    let start = index.position_from_offset(range.start().byte_offset());
                    let delta_line = start.line - pre_line;
                    let token = SemanticToken {
                        delta_line,
                        delta_start: if delta_line == 0 {
                            start.character - pre_start
                        } else {
                            start.character
                        },
                        length: index.token_length(&range),
                        token_type,
                        token_modifiers_bitset: modifiers,
                    };
                    pre_line = start.line;
                    pre_start = start.character;
                    token
                })
                .collect();

            guard.patches = patches;
            guard.defs = defs;
        }
        self.client
            .publish_diagnostics(uri, diags, Some(version))
            .await
    }
}

impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let position_encoding = Self::choose_position_encoding(&params);
        {
            let mut guard = self.config.lock().await;
            guard.root = params
                .workspace_folders
                .as_ref()
                .and_then(|folders| folders.first())
                .and_then(|workspace| uri_to_file_path(&workspace.uri))
                .map(Cow::into_owned);
            if let Some(root) = guard.root.as_deref() {
                log::info!("project root: {}", root.display())
            } else {
                log::info!("project root: <not specified>")
            }
            guard.workspaces = params
                .workspace_folders
                .as_ref()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|w| uri_to_file_path(&w.uri).map(Cow::into_owned))
                .collect();
            for workspace in guard.workspaces.iter() {
                log::info!("workspace: {}", workspace.display())
            }
        }
        *self
            .position_encoding
            .write()
            .expect("position encoding lock poisoned") = position_encoding.clone();
        Ok(InitializeResult {
            server_info: None,
            offset_encoding: (position_encoding == PositionEncodingKind::UTF8)
                .then(|| "utf-8".to_owned()),
            capabilities: ServerCapabilities {
                position_encoding: Some(position_encoding),
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(true),
                        })),
                        ..Default::default()
                    },
                )),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    ..Default::default()
                }),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensRegistrationOptions(
                        SemanticTokensRegistrationOptions {
                            text_document_registration_options: TextDocumentRegistrationOptions {
                                document_selector: Some(vec![DocumentFilter {
                                    language: Some("dolang".to_string()),
                                    scheme: Some("file".to_string()),
                                    pattern: None,
                                }]),
                            },
                            semantic_tokens_options: SemanticTokensOptions {
                                work_done_progress_options: WorkDoneProgressOptions::default(),
                                legend: SemanticTokensLegend {
                                    token_types: LEGEND_TYPES.to_vec(),
                                    token_modifiers: TOKEN_MODIFIERS.to_vec(),
                                },
                                range: Some(false),
                                full: Some(SemanticTokensFullOptions::Bool(true)),
                            },
                            static_registration_options: StaticRegistrationOptions::default(),
                        },
                    ),
                ),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
        })
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.on_change(params.text_document).await
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        self.on_change(TextDocumentItem {
            language_id: "dol".to_owned(),
            text: params.content_changes.into_iter().next().unwrap().text,
            uri: params.text_document.uri,
            version: params.text_document.version,
        })
        .await
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        if let Some(text) = params.text {
            let item = TextDocumentItem {
                language_id: "dol".to_owned(),
                uri: params.text_document.uri,
                text,
                version: -1,
            };
            self.on_change(item).await;
            _ = self.client.semantic_tokens_refresh().await;
        }
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let document = self
            .documents
            .lock()
            .await
            .get(&params.text_document.uri)
            .unwrap()
            .clone();
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            data: document.lock().await.tokens.clone(),
            ..Default::default()
        })))
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<Vec<CodeActionOrCommand>>> {
        let document = match self.documents.lock().await.get(&params.text_document.uri) {
            Some(doc) => doc.clone(),
            None => return Ok(None),
        };

        let patches = &document.lock().await.patches;
        let mut actions = Vec::new();

        for patch in patches {
            let cursor_in_diagnostic =
                range_contains_position(patch.diagnostic_range, params.range.start);
            let cursor_in_patch = range_contains_position(patch.patch_range, params.range.start);

            if cursor_in_diagnostic || cursor_in_patch {
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: patch.title.clone(),
                    kind: Some(CodeActionKind::QUICKFIX),
                    edit: Some(WorkspaceEdit {
                        changes: Some({
                            let mut changes = std::collections::HashMap::new();
                            changes.insert(
                                params.text_document.uri.clone(),
                                vec![TextEdit {
                                    range: patch.patch_range,
                                    new_text: patch.replacement.clone(),
                                }],
                            );
                            changes
                        }),
                        ..Default::default()
                    }),
                    diagnostics: Some(vec![Diagnostic {
                        range: patch.diagnostic_range,
                        severity: Some(patch.diagnostic_severity),
                        message: patch.diagnostic_message.clone(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }));
            }
        }

        Ok(Some(actions))
    }

    async fn shutdown(&self) -> Result<()> {
        log::debug!("shutting down");
        self.vm.join().await.map_err(|e| {
            let mut error = jsonrpc::Error::new(jsonrpc::ErrorCode::InternalError);
            error.message = e.into();
            error
        })?;
        Ok(())
    }

    async fn did_close(&self, _: DidCloseTextDocumentParams) {}

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        log::debug!("change config: {params:?}")
    }

    async fn did_change_workspace_folders(&self, params: DidChangeWorkspaceFoldersParams) {
        log::debug!("change workspace: {params:?}")
    }

    async fn did_change_watched_files(&self, _: DidChangeWatchedFilesParams) {}

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let document = match self
            .documents
            .lock()
            .await
            .get(&params.text_document_position_params.text_document.uri)
        {
            Some(doc) => doc.clone(),
            None => return Ok(None),
        };
        let guard = document.lock().await;
        let pos = &params.text_document_position_params.position;
        let end = guard.defs.partition_point(|(range, _)| &range.start <= pos);
        if end == 0 {
            return Ok(None);
        }
        let def = &guard.defs[end - 1];
        if &def.0.end <= pos {
            Ok(None)
        } else {
            Ok(Some(GotoDefinitionResponse::Scalar(Location {
                uri: params
                    .text_document_position_params
                    .text_document
                    .uri
                    .clone(),
                range: def.1,
            })))
        }
    }
}

pub(crate) fn build_service() -> (LspService<Backend>, ClientSocket) {
    LspService::new(Backend::new)
}

fn uri_to_file_path(uri: &Uri) -> Option<Cow<'_, Path>> {
    (uri.scheme().as_str() == "file")
        .then_some(())
        .and_then(|()| uri.to_file_path())
}

fn range_contains_position(range: Range, position: Position) -> bool {
    (range.start.line < position.line
        || (range.start.line == position.line && range.start.character <= position.character))
        && (range.end.line > position.line
            || (range.end.line == position.line && range.end.character >= position.character))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::{SinkExt, StreamExt};
    use serde::{Serialize, de::DeserializeOwned};
    use serde_json::{Value, json};
    use tower::{Service, ServiceExt};
    use tower_lsp_server::jsonrpc::{Request, Response};
    use tower_lsp_server::ls_types::{notification, request};
    use tower_lsp_server::ls_types::{notification::Notification as _, request::Request as _};

    use super::*;

    struct Harness {
        service: LspService<Backend>,
        socket: ClientSocket,
        next_id: i64,
    }

    impl Harness {
        fn new() -> Self {
            let (service, socket) = build_service();
            Self {
                service,
                socket,
                next_id: 1,
            }
        }

        async fn send_request<R>(&mut self, params: R::Params) -> R::Result
        where
            R: request::Request,
            R::Params: Serialize,
            R::Result: DeserializeOwned,
        {
            let id = self.next_id;
            self.next_id += 1;
            let response = self
                .service
                .ready()
                .await
                .unwrap()
                .call(
                    Request::build(R::METHOD)
                        .params(serde_json::to_value(params).unwrap())
                        .id(id)
                        .finish(),
                )
                .await
                .unwrap()
                .unwrap();
            let (_, body) = response.into_parts();
            serde_json::from_value(body.unwrap()).unwrap()
        }

        async fn send_notification<N>(&mut self, params: N::Params)
        where
            N: notification::Notification,
            N::Params: Serialize,
        {
            let response = self
                .service
                .ready()
                .await
                .unwrap()
                .call(
                    Request::build(N::METHOD)
                        .params(serde_json::to_value(params).unwrap())
                        .finish(),
                )
                .await
                .unwrap();
            assert!(response.is_none());
        }

        async fn next_client_request(&mut self) -> Request {
            tokio::time::timeout(Duration::from_secs(1), self.socket.next())
                .await
                .unwrap()
                .unwrap()
        }

        async fn next_client_notification<N>(&mut self) -> N::Params
        where
            N: notification::Notification,
            N::Params: DeserializeOwned,
        {
            let request = self.next_client_request().await;
            assert_eq!(request.method(), N::METHOD);
            assert!(request.id().is_none());
            serde_json::from_value(request.params().cloned().unwrap_or(Value::Null)).unwrap()
        }

        async fn initialize(&mut self, offered: Vec<PositionEncodingKind>) -> InitializeResult {
            let result = self
                .send_request::<request::Initialize>(InitializeParams {
                    capabilities: ClientCapabilities {
                        general: Some(GeneralClientCapabilities {
                            position_encodings: Some(offered),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .await;
            self.send_notification::<notification::Initialized>(InitializedParams {})
                .await;
            result
        }

        async fn open(&mut self, uri: Uri, text: &str, version: i32) -> PublishDiagnosticsParams {
            self.send_notification::<notification::DidOpenTextDocument>(
                DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri,
                        language_id: "dolang".to_owned(),
                        version,
                        text: text.to_owned(),
                    },
                },
            )
            .await;
            self.next_client_notification::<notification::PublishDiagnostics>()
                .await
        }
    }

    #[test]
    fn choose_utf8_when_client_offers_it() {
        let params = InitializeParams {
            capabilities: ClientCapabilities {
                general: Some(GeneralClientCapabilities {
                    position_encodings: Some(vec![
                        PositionEncodingKind::UTF16,
                        PositionEncodingKind::UTF8,
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(
            Backend::choose_position_encoding(&params),
            PositionEncodingKind::UTF8
        );
    }

    #[test]
    fn default_to_utf16_when_utf8_not_offered() {
        let params = InitializeParams {
            capabilities: ClientCapabilities {
                general: Some(GeneralClientCapabilities {
                    position_encodings: Some(vec![PositionEncodingKind::UTF16]),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(
            Backend::choose_position_encoding(&params),
            PositionEncodingKind::UTF16
        );
    }

    #[test]
    fn utf8_positions_use_byte_offsets_within_line() {
        let content = "a😀b\nx";
        let index = DocumentIndex::new(content, PositionEncodingKind::UTF8);
        let offset = content.find('b').unwrap();

        assert_eq!(index.position_from_offset(offset), Position::new(0, 5));
    }

    #[test]
    fn utf16_positions_count_code_units_within_line() {
        let content = "a😀b\nx";
        let index = DocumentIndex::new(content, PositionEncodingKind::UTF16);
        let offset = content.find('b').unwrap();

        assert_eq!(index.position_from_offset(offset), Position::new(0, 3));
    }

    #[test]
    fn utf16_token_length_counts_code_units() {
        let content = "😀x";
        let index = DocumentIndex::new(content, PositionEncodingKind::UTF16);

        assert_eq!(index.token_length_from_offsets(0, "😀".len()), 2);
    }

    #[test]
    fn utf16_range_handles_mixed_content_lines() {
        let content = "pre😀fix\nsecond";
        let start = content.find("fix").unwrap();
        let end = start + "fix".len();
        let index = DocumentIndex::new(content, PositionEncodingKind::UTF16);

        assert_eq!(
            index.range_from_offsets(start, end),
            Range::new(Position::new(0, 5), Position::new(0, 8))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn initialize_advertises_requested_position_encoding() {
        let mut harness = Harness::new();
        let result = harness
            .initialize(vec![
                PositionEncodingKind::UTF16,
                PositionEncodingKind::UTF8,
            ])
            .await;

        assert_eq!(
            result.capabilities.position_encoding,
            Some(PositionEncodingKind::UTF8)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn did_open_publishes_diagnostics() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///diagnostic-test.dol".parse().unwrap();
        let diagnostics = harness.open(uri, "\"\\q\"", 1).await;

        assert_eq!(diagnostics.diagnostics.len(), 1);
        assert_eq!(
            diagnostics.diagnostics[0].message,
            "unexpected escape sequence"
        );
        assert_eq!(
            diagnostics.diagnostics[0].range,
            Range::new(Position::new(0, 1), Position::new(0, 3))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn semantic_tokens_use_utf8_and_utf16_lengths() {
        let source = "# 😀\n";
        let uri: Uri = "file:///semantic-token-test.dol".parse().unwrap();

        let mut utf8 = Harness::new();
        utf8.initialize(vec![PositionEncodingKind::UTF8]).await;
        utf8.open(uri.clone(), source, 1).await;
        let utf8_tokens = utf8
            .send_request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                text_document: TextDocumentIdentifier { uri: uri.clone() },
            })
            .await
            .unwrap();

        let mut utf16 = Harness::new();
        utf16.initialize(vec![PositionEncodingKind::UTF16]).await;
        utf16.open(uri, source, 1).await;
        let utf16_tokens = utf16
            .send_request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                text_document: TextDocumentIdentifier {
                    uri: "file:///semantic-token-test.dol".parse().unwrap(),
                },
            })
            .await
            .unwrap();

        let utf8_data = match utf8_tokens {
            SemanticTokensResult::Tokens(tokens) => tokens.data,
            SemanticTokensResult::Partial(_) => panic!("unexpected partial tokens"),
        };
        let utf16_data = match utf16_tokens {
            SemanticTokensResult::Tokens(tokens) => tokens.data,
            SemanticTokensResult::Partial(_) => panic!("unexpected partial tokens"),
        };

        assert_eq!(utf8_data.len(), 1);
        assert_eq!(utf16_data.len(), 1);
        assert_eq!(utf8_data[0].length, 6);
        assert_eq!(utf16_data[0].length, 4);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn goto_definition_and_code_action_round_trip() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///definition-code-action-test.dol".parse().unwrap();
        let source = "let x = 5\nx\necho $x\n";
        let diagnostics = harness.open(uri.clone(), source, 1).await;

        assert!(!diagnostics.diagnostics.is_empty());
        assert_eq!(
            diagnostics.diagnostics[0].message,
            "statement with no effect"
        );

        let definition = harness
            .send_request::<request::GotoDefinition>(GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position: Position::new(2, 6),
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await
            .unwrap();

        let location = match definition {
            GotoDefinitionResponse::Scalar(location) => location,
            _ => panic!("unexpected definition response shape"),
        };
        assert_eq!(
            location.range,
            Range::new(Position::new(0, 4), Position::new(0, 5))
        );

        let actions = harness
            .send_request::<request::CodeActionRequest>(CodeActionParams {
                text_document: TextDocumentIdentifier { uri },
                range: Range::new(Position::new(1, 0), Position::new(1, 0)),
                context: CodeActionContext {
                    diagnostics: diagnostics.diagnostics,
                    only: None,
                    trigger_kind: None,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await
            .unwrap();

        let action = match &actions[0] {
            CodeActionOrCommand::CodeAction(action) => action,
            CodeActionOrCommand::Command(_) => panic!("unexpected command"),
        };
        assert_eq!(action.title, "add () to make this a call");
        let edit = action.edit.as_ref().unwrap();
        let changes = edit.changes.as_ref().unwrap();
        let edits = changes.values().next().unwrap();
        assert_eq!(
            edits[0].range,
            Range::new(Position::new(1, 0), Position::new(1, 1))
        );
        assert_eq!(edits[0].new_text, "x()");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn did_save_requests_semantic_token_refresh() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///save-refresh-test.dol".parse().unwrap();
        harness.open(uri.clone(), "def foo = 42\n", 1).await;

        let request = Request::build(notification::DidSaveTextDocument::METHOD)
            .params(json!(DidSaveTextDocumentParams {
                text: Some("def foo = 42\n".to_owned()),
                text_document: TextDocumentIdentifier { uri },
            }))
            .finish();
        let service = &mut harness.service;
        let socket = &mut harness.socket;

        let save = async move { service.ready().await.unwrap().call(request).await.unwrap() };
        let observe = async move {
            let published = tokio::time::timeout(Duration::from_secs(1), socket.next())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(published.method(), notification::PublishDiagnostics::METHOD);

            let refresh = tokio::time::timeout(Duration::from_secs(1), socket.next())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(refresh.method(), request::SemanticTokensRefresh::METHOD);

            let (_, id, _) = refresh.into_parts();
            socket
                .send(Response::from_ok(id.unwrap(), Value::Null))
                .await
                .unwrap();
        };

        let (save, ()) = tokio::join!(save, observe);
        assert_eq!(save, None);
    }
}
