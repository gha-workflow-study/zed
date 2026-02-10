use collections::HashMap;
use futures::future::join_all;
use gpui::{Context, Task, Window};
use itertools::Itertools as _;
use language::language_settings::language_settings;
use language::{Buffer, OutlineItem};
use multi_buffer::{Anchor, MultiBufferSnapshot};
use text::BufferId;
use theme::ActiveTheme as _;

use crate::{Editor, LSP_REQUEST_DEBOUNCE_TIMEOUT};

impl Editor {
    /// Returns all document outline items for a buffer, using LSP or
    /// tree-sitter based on the `document_symbols` setting.
    /// External consumers (outline modal, outline panel, breadcrumbs) should use this.
    pub fn buffer_outline_items(
        &self,
        buffer_id: BufferId,
        cx: &gpui::App,
    ) -> Task<Vec<OutlineItem<text::Anchor>>> {
        let Some(buffer) = self.buffer.read(cx).buffer(buffer_id) else {
            return Task::ready(Vec::new());
        };

        if Self::lsp_symbols_enabled(buffer.read(cx), cx) {
            let items = self
                .lsp_document_symbols
                .get(&buffer_id)
                .cloned()
                .unwrap_or_default();
            return Task::ready(items);
        }

        let buffer_snapshot = buffer.read(cx).snapshot();
        let syntax = cx.theme().syntax().clone();
        cx.background_executor()
            .spawn(async move { buffer_snapshot.outline(Some(&syntax)).items })
    }

    fn lsp_symbols_enabled(buffer: &Buffer, cx: &gpui::App) -> bool {
        language_settings(buffer.language().map(|l| l.name()), buffer.file(), cx)
            .document_symbols
            .lsp_enabled()
    }

    /// Whether the buffer at `cursor` has LSP document symbols enabled.
    pub(super) fn uses_lsp_document_symbols(
        &self,
        cursor: Anchor,
        multibuffer_snapshot: &MultiBufferSnapshot,
        cx: &Context<Self>,
    ) -> bool {
        let Some(excerpt) = multibuffer_snapshot.excerpt_containing(cursor..cursor) else {
            return false;
        };
        let Some(buffer) = self.buffer.read(cx).buffer(excerpt.buffer_id()) else {
            return false;
        };
        Self::lsp_symbols_enabled(buffer.read(cx), cx)
    }

    /// Filters editor-local LSP document symbols to the ancestor chain
    /// containing `cursor`. Never triggers an LSP request.
    pub(super) fn lsp_symbols_at_cursor(
        &self,
        cursor: Anchor,
        multibuffer_snapshot: &MultiBufferSnapshot,
        cx: &Context<Self>,
    ) -> Option<(BufferId, Vec<OutlineItem<Anchor>>)> {
        let excerpt = multibuffer_snapshot.excerpt_containing(cursor..cursor)?;
        let excerpt_id = excerpt.id();
        let buffer_id = excerpt.buffer_id();
        let buffer = self.buffer.read(cx).buffer(buffer_id)?;
        let buffer_snapshot = buffer.read(cx).snapshot();
        let cursor_text_anchor = cursor.text_anchor;

        let all_items = self.lsp_document_symbols.get(&buffer_id)?;
        if all_items.is_empty() {
            return None;
        }

        let mut symbols: Vec<OutlineItem<Anchor>> = all_items
            .iter()
            .filter(|item| {
                item.range
                    .start
                    .cmp(&cursor_text_anchor, &buffer_snapshot)
                    .is_le()
                    && item
                        .range
                        .end
                        .cmp(&cursor_text_anchor, &buffer_snapshot)
                        .is_ge()
            })
            .map(|item| OutlineItem {
                depth: item.depth,
                range: Anchor::range_in_buffer(excerpt_id, item.range.clone()),
                source_range_for_text: Anchor::range_in_buffer(
                    excerpt_id,
                    item.source_range_for_text.clone(),
                ),
                text: item.text.clone(),
                highlight_ranges: item.highlight_ranges.clone(),
                name_ranges: item.name_ranges.clone(),
                body_range: item
                    .body_range
                    .as_ref()
                    .map(|r| Anchor::range_in_buffer(excerpt_id, r.clone())),
                annotation_range: item
                    .annotation_range
                    .as_ref()
                    .map(|r| Anchor::range_in_buffer(excerpt_id, r.clone())),
            })
            .collect();

        let mut prev_depth = None;
        symbols.retain(|item| {
            let result = prev_depth.is_none_or(|prev_depth| item.depth > prev_depth);
            prev_depth = Some(item.depth);
            result
        });

        Some((buffer_id, symbols))
    }

    /// Fetches document symbols from the LSP for buffers that have the setting
    /// enabled. Called from `update_lsp_data` on edits, server events, etc.
    /// When the fetch completes, stores results in `self.lsp_document_symbols`
    /// and triggers `refresh_outline_symbols_at_cursor` so breadcrumbs pick up the new data.
    pub(super) fn refresh_document_symbols(
        &mut self,
        for_buffer: Option<BufferId>,
        _window: &Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mode().is_full() {
            return;
        }
        let Some(project) = self.project.clone() else {
            return;
        };

        let buffers_to_query = self
            .buffer
            .read(cx)
            .all_buffers()
            .into_iter()
            .filter(|buffer| {
                let buffer = buffer.read(cx);
                let id = buffer.remote_id();
                for_buffer.is_none_or(|target| target == id)
                    && Self::lsp_symbols_enabled(buffer, cx)
            })
            .unique_by(|buffer| buffer.read(cx).remote_id())
            .collect::<Vec<_>>();

        if buffers_to_query.is_empty() {
            return;
        }

        self.refresh_document_symbols_task = cx.spawn(async move |editor, cx| {
            cx.background_executor()
                .timer(LSP_REQUEST_DEBOUNCE_TIMEOUT)
                .await;

            let Some(tasks) = editor
                .update(cx, |_, cx| {
                    project.read(cx).lsp_store().update(cx, |lsp_store, cx| {
                        buffers_to_query
                            .into_iter()
                            .map(|buffer| {
                                let buffer_id = buffer.read(cx).remote_id();
                                let task = lsp_store.fetch_document_symbols(&buffer, cx);
                                async move { (buffer_id, task.await) }
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .ok()
            else {
                return;
            };

            let results: HashMap<BufferId, Vec<OutlineItem<text::Anchor>>> =
                join_all(tasks).await.into_iter().collect();

            editor
                .update(cx, |editor, cx| {
                    editor.lsp_document_symbols.extend(results);
                    editor.refresh_outline_symbols_at_cursor(cx);
                })
                .ok();
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic};

    use futures::StreamExt as _;
    use gpui::TestAppContext;
    use settings::DocumentSymbols;
    use util::path;
    use zed_actions::editor::{MoveDown, MoveUp};

    use crate::{
        editor_tests::{init_test, update_test_language_settings},
        test::editor_lsp_test_context::EditorLspTestContext,
    };

    fn lsp_range(start_line: u32, start_char: u32, end_line: u32, end_char: u32) -> lsp::Range {
        lsp::Range {
            start: lsp::Position::new(start_line, start_char),
            end: lsp::Position::new(end_line, end_char),
        }
    }

    fn nested_symbol(
        name: &str,
        kind: lsp::SymbolKind,
        range: lsp::Range,
        selection_range: lsp::Range,
        children: Vec<lsp::DocumentSymbol>,
    ) -> lsp::DocumentSymbol {
        #[allow(deprecated)]
        lsp::DocumentSymbol {
            name: name.to_string(),
            detail: None,
            kind,
            tags: None,
            deprecated: None,
            range,
            selection_range,
            children: if children.is_empty() {
                None
            } else {
                Some(children)
            },
        }
    }

    #[gpui::test]
    async fn test_lsp_document_symbols_fetches_when_enabled(cx: &mut TestAppContext) {
        init_test(cx, |_| {});

        update_test_language_settings(cx, |settings| {
            settings.defaults.document_symbols = Some(DocumentSymbols::On);
        });

        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                document_symbol_provider: Some(lsp::OneOf::Left(true)),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await;

        let mut symbol_request = cx
            .set_request_handler::<lsp::request::DocumentSymbolRequest, _, _>(
                move |_, _, _| async move {
                    Ok(Some(lsp::DocumentSymbolResponse::Nested(vec![
                        nested_symbol(
                            "main",
                            lsp::SymbolKind::FUNCTION,
                            lsp_range(0, 0, 2, 1),
                            lsp_range(0, 3, 0, 7),
                            Vec::new(),
                        ),
                    ])))
                },
            );

        cx.set_state("fn maˇin() {\n    let x = 1;\n}\n");
        cx.run_until_parked();

        // Trigger breadcrumb refresh by notifying
        cx.update_editor(|editor, _window, cx| {
            let _symbols = &editor.outline_symbols_at_cursor;
            cx.notify();
        });

        // The selection change triggers refresh_outline_symbols_at_cursor
        assert!(symbol_request.next().await.is_some());
        cx.run_until_parked();

        cx.update_editor(|editor, _window, _cx| {
            let symbols = editor
                .outline_symbols_at_cursor
                .as_ref()
                .expect("Should have outline symbols after LSP response");
            let names: Vec<&str> = symbols.1.iter().map(|s| s.text.as_str()).collect();
            assert_eq!(names, vec!["main"]);
        });
    }

    #[gpui::test]
    async fn test_lsp_document_symbols_nested(cx: &mut TestAppContext) {
        init_test(cx, |_| {});

        update_test_language_settings(cx, |settings| {
            settings.defaults.document_symbols = Some(DocumentSymbols::On);
        });

        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                document_symbol_provider: Some(lsp::OneOf::Left(true)),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await;

        let mut symbol_request = cx
            .set_request_handler::<lsp::request::DocumentSymbolRequest, _, _>(
                move |_, _, _| async move {
                    Ok(Some(lsp::DocumentSymbolResponse::Nested(vec![
                        nested_symbol(
                            "Foo",
                            lsp::SymbolKind::STRUCT,
                            lsp_range(0, 0, 3, 1),
                            lsp_range(0, 7, 0, 10),
                            vec![
                                nested_symbol(
                                    "bar",
                                    lsp::SymbolKind::FIELD,
                                    lsp_range(1, 4, 1, 13),
                                    lsp_range(1, 4, 1, 7),
                                    Vec::new(),
                                ),
                                nested_symbol(
                                    "baz",
                                    lsp::SymbolKind::FIELD,
                                    lsp_range(2, 4, 2, 15),
                                    lsp_range(2, 4, 2, 7),
                                    Vec::new(),
                                ),
                            ],
                        ),
                    ])))
                },
            );

        cx.set_state("struct Foo {\n    baˇr: u32,\n    baz: String,\n}\n");
        assert!(symbol_request.next().await.is_some());
        cx.run_until_parked();

        cx.update_editor(|editor, _window, _cx| {
            let symbols = editor
                .outline_symbols_at_cursor
                .as_ref()
                .expect("Should have outline symbols");
            let names: Vec<&str> = symbols.1.iter().map(|s| s.text.as_str()).collect();
            // cursor is inside Foo > bar, so we expect the containing chain
            assert_eq!(names, vec!["Foo", "bar"]);
        });
    }

    #[gpui::test]
    async fn test_lsp_document_symbols_switch_tree_sitter_to_lsp_and_back(cx: &mut TestAppContext) {
        init_test(cx, |_| {});

        // Start with tree-sitter (default)
        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                document_symbol_provider: Some(lsp::OneOf::Left(true)),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await;

        let mut symbol_request = cx
            .set_request_handler::<lsp::request::DocumentSymbolRequest, _, _>(
                move |_, _, _| async move {
                    Ok(Some(lsp::DocumentSymbolResponse::Nested(vec![
                        nested_symbol(
                            "lsp_main_symbol",
                            lsp::SymbolKind::FUNCTION,
                            lsp_range(0, 0, 2, 1),
                            lsp_range(0, 3, 0, 7),
                            Vec::new(),
                        ),
                    ])))
                },
            );

        cx.set_state("fn maˇin() {\n    let x = 1;\n}\n");
        cx.run_until_parked();

        // Step 1: With tree-sitter (default), breadcrumbs use tree-sitter outline
        cx.update_editor(|editor, _window, _cx| {
            let symbols = editor
                .outline_symbols_at_cursor
                .as_ref()
                .expect("Should have tree-sitter outline symbols");
            let names: Vec<&str> = symbols.1.iter().map(|s| s.text.as_str()).collect();
            assert_eq!(
                names,
                vec!["fn main"],
                "Tree-sitter should produce 'fn main'"
            );
        });

        // Step 2: Switch to LSP
        update_test_language_settings(&mut cx.cx.cx, |settings| {
            settings.defaults.document_symbols = Some(DocumentSymbols::On);
        });
        cx.run_until_parked();

        // Force a selection change to trigger refresh_outline_symbols_at_cursor
        cx.update_editor(|editor, window, cx| {
            editor.move_down(&MoveDown, window, cx);
        });
        assert!(symbol_request.next().await.is_some());
        cx.run_until_parked();

        cx.update_editor(|editor, _window, _cx| {
            let symbols = editor
                .outline_symbols_at_cursor
                .as_ref()
                .expect("Should have LSP outline symbols after switching to LSP");
            let names: Vec<&str> = symbols.1.iter().map(|s| s.text.as_str()).collect();
            assert_eq!(
                names,
                vec!["lsp_main_symbol"],
                "After switching to LSP, should see LSP symbols"
            );
        });

        // Step 3: Switch back to tree-sitter
        update_test_language_settings(&mut cx.cx.cx, |settings| {
            settings.defaults.document_symbols = Some(DocumentSymbols::Off);
        });
        cx.run_until_parked();

        // Force another selection change
        cx.update_editor(|editor, window, cx| {
            editor.move_up(&MoveUp, window, cx);
        });
        cx.run_until_parked();

        cx.update_editor(|editor, _window, _cx| {
            let symbols = editor
                .outline_symbols_at_cursor
                .as_ref()
                .expect("Should have tree-sitter symbols after switching back");
            let names: Vec<&str> = symbols.1.iter().map(|s| s.text.as_str()).collect();
            assert_eq!(
                names,
                vec!["fn main"],
                "After switching back to tree-sitter, should see tree-sitter symbols again"
            );
        });
    }

    #[gpui::test]
    async fn test_lsp_document_symbols_caches_results(cx: &mut TestAppContext) {
        init_test(cx, |_| {});

        update_test_language_settings(cx, |settings| {
            settings.defaults.document_symbols = Some(DocumentSymbols::On);
        });

        let request_count = Arc::new(atomic::AtomicUsize::new(0));
        let request_count_clone = request_count.clone();

        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                document_symbol_provider: Some(lsp::OneOf::Left(true)),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await;

        let mut symbol_request = cx
            .set_request_handler::<lsp::request::DocumentSymbolRequest, _, _>(move |_, _, _| {
                request_count_clone.fetch_add(1, atomic::Ordering::AcqRel);
                async move {
                    Ok(Some(lsp::DocumentSymbolResponse::Nested(vec![
                        nested_symbol(
                            "main",
                            lsp::SymbolKind::FUNCTION,
                            lsp_range(0, 0, 2, 1),
                            lsp_range(0, 3, 0, 7),
                            Vec::new(),
                        ),
                    ])))
                }
            });

        cx.set_state("fn maˇin() {\n    let x = 1;\n}\n");
        assert!(symbol_request.next().await.is_some());
        cx.run_until_parked();

        let first_count = request_count.load(atomic::Ordering::Acquire);
        assert!(first_count > 0, "Should have made at least one request");

        // Move cursor within the same buffer version — should use cache
        cx.update_editor(|editor, window, cx| {
            editor.move_down(&MoveDown, window, cx);
        });
        cx.run_until_parked();

        let second_count = request_count.load(atomic::Ordering::Acquire);
        assert_eq!(
            first_count, second_count,
            "Moving cursor without editing should use cached symbols"
        );
    }

    #[gpui::test]
    async fn test_lsp_document_symbols_flat_response(cx: &mut TestAppContext) {
        init_test(cx, |_| {});

        update_test_language_settings(cx, |settings| {
            settings.defaults.document_symbols = Some(DocumentSymbols::On);
        });

        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                document_symbol_provider: Some(lsp::OneOf::Left(true)),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await;

        let mut symbol_request = cx
            .set_request_handler::<lsp::request::DocumentSymbolRequest, _, _>(
                move |_, _, _| async move {
                    #[allow(deprecated)]
                    Ok(Some(lsp::DocumentSymbolResponse::Flat(vec![
                        lsp::SymbolInformation {
                            name: "main".to_string(),
                            kind: lsp::SymbolKind::FUNCTION,
                            tags: None,
                            deprecated: None,
                            location: lsp::Location {
                                uri: lsp::Uri::from_file_path(path!("/a/main.rs")).unwrap(),
                                range: lsp_range(0, 0, 2, 1),
                            },
                            container_name: None,
                        },
                    ])))
                },
            );

        cx.set_state("fn maˇin() {\n    let x = 1;\n}\n");
        assert!(symbol_request.next().await.is_some());
        cx.run_until_parked();

        cx.update_editor(|editor, _window, _cx| {
            let symbols = editor
                .outline_symbols_at_cursor
                .as_ref()
                .expect("Should have outline symbols from flat response");
            let names: Vec<&str> = symbols.1.iter().map(|s| s.text.as_str()).collect();
            assert_eq!(names, vec!["main"]);
        });
    }

    #[gpui::test]
    async fn test_breadcrumbs_use_lsp_symbols(cx: &mut TestAppContext) {
        init_test(cx, |_| {});

        update_test_language_settings(cx, |settings| {
            settings.defaults.document_symbols = Some(DocumentSymbols::On);
        });

        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                document_symbol_provider: Some(lsp::OneOf::Left(true)),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await;

        let mut symbol_request = cx
            .set_request_handler::<lsp::request::DocumentSymbolRequest, _, _>(
                move |_, _, _| async move {
                    Ok(Some(lsp::DocumentSymbolResponse::Nested(vec![
                        nested_symbol(
                            "MyModule",
                            lsp::SymbolKind::MODULE,
                            lsp_range(0, 0, 4, 1),
                            lsp_range(0, 4, 0, 12),
                            vec![nested_symbol(
                                "my_function",
                                lsp::SymbolKind::FUNCTION,
                                lsp_range(1, 4, 3, 5),
                                lsp_range(1, 7, 1, 18),
                                Vec::new(),
                            )],
                        ),
                    ])))
                },
            );

        cx.set_state("mod MyModule {\n    fn my_fuˇnction() {\n        let x = 1;\n    }\n}\n");
        assert!(symbol_request.next().await.is_some());
        cx.run_until_parked();

        cx.update_editor(|editor, _window, _cx| {
            let (_buffer_id, symbols) = editor
                .outline_symbols_at_cursor
                .as_ref()
                .expect("Should have outline symbols from LSP");
            let names: Vec<&str> = symbols.iter().map(|s| s.text.as_str()).collect();
            assert_eq!(names, vec!["MyModule", "my_function"]);
        });
    }

    #[gpui::test]
    async fn test_lsp_document_symbols_empty_response(cx: &mut TestAppContext) {
        init_test(cx, |_| {});

        update_test_language_settings(cx, |settings| {
            settings.defaults.document_symbols = Some(DocumentSymbols::On);
        });

        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                document_symbol_provider: Some(lsp::OneOf::Left(true)),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await;

        let mut symbol_request = cx
            .set_request_handler::<lsp::request::DocumentSymbolRequest, _, _>(
                move |_, _, _| async move {
                    Ok(Some(lsp::DocumentSymbolResponse::Nested(Vec::new())))
                },
            );

        cx.set_state("fn maˇin() {\n    let x = 1;\n}\n");
        assert!(symbol_request.next().await.is_some());
        cx.run_until_parked();

        cx.update_editor(|editor, _window, _cx| {
            // With LSP enabled but empty response, outline_symbols_at_cursor should be None
            // (no symbols to show in breadcrumbs)
            assert!(
                editor.outline_symbols_at_cursor.is_none(),
                "Empty LSP response should result in no outline symbols"
            );
        });
    }

    #[gpui::test]
    async fn test_lsp_document_symbols_disabled_by_default(cx: &mut TestAppContext) {
        init_test(cx, |_| {});

        // Do NOT enable document_symbols — defaults to Off

        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                document_symbol_provider: Some(lsp::OneOf::Left(true)),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await;

        let request_count = Arc::new(atomic::AtomicUsize::new(0));
        let request_count_clone = request_count.clone();

        let _symbol_request =
            cx.set_request_handler::<lsp::request::DocumentSymbolRequest, _, _>(move |_, _, _| {
                request_count_clone.fetch_add(1, atomic::Ordering::AcqRel);
                async move {
                    Ok(Some(lsp::DocumentSymbolResponse::Nested(vec![
                        nested_symbol(
                            "should_not_appear",
                            lsp::SymbolKind::FUNCTION,
                            lsp_range(0, 0, 2, 1),
                            lsp_range(0, 3, 0, 7),
                            Vec::new(),
                        ),
                    ])))
                }
            });

        cx.set_state("fn maˇin() {\n    let x = 1;\n}\n");
        cx.run_until_parked();

        // Tree-sitter should be used instead
        cx.update_editor(|editor, _window, _cx| {
            let symbols = editor
                .outline_symbols_at_cursor
                .as_ref()
                .expect("Should have tree-sitter outline symbols");
            let names: Vec<&str> = symbols.1.iter().map(|s| s.text.as_str()).collect();
            assert_eq!(
                names,
                vec!["fn main"],
                "With document_symbols off, should use tree-sitter"
            );
        });

        assert_eq!(
            request_count.load(atomic::Ordering::Acquire),
            0,
            "Should not have made any LSP document symbol requests when setting is off"
        );
    }
}
