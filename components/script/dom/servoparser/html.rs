/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#![cfg_attr(crown, allow(crown::unrooted_must_root))]

use std::cell::Cell;
use std::io;

use html5ever::QualName;
use html5ever::buffer_queue::BufferQueue;
use html5ever::serialize::TraversalScope::IncludeNode;
use html5ever::serialize::{AttrRef, Serialize, Serializer, TraversalScope};
use html5ever::tokenizer::{Tokenizer as HtmlTokenizer, TokenizerOpts, TokenizerResult};
use html5ever::tree_builder::{TreeBuilder, TreeBuilderOpts};
use script_bindings::trace::CustomTraceable;
use servo_url::ServoUrl;

use crate::dom::bindings::codegen::Bindings::HTMLTemplateElementBinding::HTMLTemplateElementMethods;
use crate::dom::bindings::inheritance::{Castable, CharacterDataTypeId, NodeTypeId};
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::characterdata::CharacterData;
use crate::dom::document::Document;
use crate::dom::documentfragment::DocumentFragment;
use crate::dom::documenttype::DocumentType;
use crate::dom::element::Element;
use crate::dom::htmlscriptelement::HTMLScriptElement;
use crate::dom::htmltemplateelement::HTMLTemplateElement;
use crate::dom::node::Node;
use crate::dom::processinginstruction::ProcessingInstruction;
use crate::dom::servoparser::{ParsingAlgorithm, Sink};
use crate::script_runtime::CanGc;

#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
pub(crate) struct Tokenizer {
    #[ignore_malloc_size_of = "Defined in html5ever"]
    inner: HtmlTokenizer<TreeBuilder<Dom<Node>, Sink>>,
}

impl Tokenizer {
    pub(crate) fn new(
        document: &Document,
        url: ServoUrl,
        fragment_context: Option<super::FragmentContext>,
        parsing_algorithm: ParsingAlgorithm,
    ) -> Self {
        let sink = Sink {
            base_url: url,
            document: Dom::from_ref(document),
            current_line: Cell::new(1),
            script: Default::default(),
            parsing_algorithm,
        };

        let options = TreeBuilderOpts {
            ignore_missing_rules: true,
            scripting_enabled: document.has_browsing_context(),
            ..Default::default()
        };

        let inner = if let Some(fc) = fragment_context {
            let tb = TreeBuilder::new_for_fragment(
                sink,
                Dom::from_ref(fc.context_elem),
                fc.form_elem.map(Dom::from_ref),
                options,
            );

            let tok_options = TokenizerOpts {
                initial_state: Some(tb.tokenizer_state_for_context_elem()),
                ..Default::default()
            };

            HtmlTokenizer::new(tb, tok_options)
        } else {
            HtmlTokenizer::new(TreeBuilder::new(sink, options), Default::default())
        };

        Tokenizer { inner }
    }

    pub(crate) fn feed(&self, input: &BufferQueue) -> TokenizerResult<DomRoot<HTMLScriptElement>> {
        match self.inner.feed(input) {
            TokenizerResult::Done => TokenizerResult::Done,
            TokenizerResult::Script(script) => {
                TokenizerResult::Script(DomRoot::from_ref(script.downcast().unwrap()))
            },
        }
    }

    pub(crate) fn end(&self) {
        self.inner.end();
    }

    pub(crate) fn url(&self) -> &ServoUrl {
        &self.inner.sink.sink.base_url
    }

    pub(crate) fn set_plaintext_state(&self) {
        self.inner.set_plaintext_state();
    }
}

fn start_element<S: Serializer>(node: &Element, serializer: &mut S) -> io::Result<()> {
    let name = QualName::new(None, node.namespace().clone(), node.local_name().clone());
    let attrs = node
        .attrs()
        .iter()
        .map(|attr| {
            let qname = QualName::new(None, attr.namespace().clone(), attr.local_name().clone());
            let value = attr.value().clone();
            (qname, value)
        })
        .collect::<Vec<_>>();
    let attr_refs = attrs.iter().map(|(qname, value)| {
        let ar: AttrRef = (qname, &**value);
        ar
    });
    serializer.start_elem(name, attr_refs)?;
    Ok(())
}

fn end_element<S: Serializer>(node: &Element, serializer: &mut S) -> io::Result<()> {
    let name = QualName::new(None, node.namespace().clone(), node.local_name().clone());
    serializer.end_elem(name)
}

enum SerializationCommand {
    OpenElement(DomRoot<Element>),
    CloseElement(DomRoot<Element>),
    SerializeNonelement(DomRoot<Node>),
}

struct SerializationIterator {
    stack: Vec<SerializationCommand>,
}

fn rev_children_iter(n: &Node, can_gc: CanGc) -> impl Iterator<Item = DomRoot<Node>> + use<'_> {
    if n.downcast::<Element>().is_some_and(|e| e.is_void()) {
        return Node::new_document_node().rev_children();
    }

    match n.downcast::<HTMLTemplateElement>() {
        Some(t) => t.Content(can_gc).upcast::<Node>().rev_children(),
        None => n.rev_children(),
    }
}

impl SerializationIterator {
    fn new(node: &Node, skip_first: bool, can_gc: CanGc) -> SerializationIterator {
        let mut ret = SerializationIterator { stack: vec![] };
        if skip_first || node.is::<DocumentFragment>() || node.is::<Document>() {
            for c in rev_children_iter(node, can_gc) {
                ret.push_node(&c);
            }
        } else {
            ret.push_node(node);
        }
        ret
    }

    fn push_node(&mut self, n: &Node) {
        match n.downcast::<Element>() {
            Some(e) => self
                .stack
                .push(SerializationCommand::OpenElement(DomRoot::from_ref(e))),
            None => self.stack.push(SerializationCommand::SerializeNonelement(
                DomRoot::from_ref(n),
            )),
        }
    }
}

impl Iterator for SerializationIterator {
    type Item = SerializationCommand;

    fn next(&mut self) -> Option<SerializationCommand> {
        let res = self.stack.pop();

        if let Some(SerializationCommand::OpenElement(ref e)) = res {
            self.stack
                .push(SerializationCommand::CloseElement(e.clone()));
            for c in rev_children_iter(e.upcast::<Node>(), CanGc::note()) {
                self.push_node(&c);
            }
        }

        res
    }
}

impl Serialize for &Node {
    fn serialize<S: Serializer>(
        &self,
        serializer: &mut S,
        traversal_scope: TraversalScope,
    ) -> io::Result<()> {
        let node = *self;

        let iter = SerializationIterator::new(node, traversal_scope != IncludeNode, CanGc::note());

        for cmd in iter {
            match cmd {
                SerializationCommand::OpenElement(n) => {
                    start_element(&n, serializer)?;
                },

                SerializationCommand::CloseElement(n) => {
                    end_element(&n, serializer)?;
                },

                SerializationCommand::SerializeNonelement(n) => match n.type_id() {
                    NodeTypeId::DocumentType => {
                        let doctype = n.downcast::<DocumentType>().unwrap();
                        serializer.write_doctype(doctype.name())?;
                    },

                    NodeTypeId::CharacterData(CharacterDataTypeId::Text(_)) => {
                        let cdata = n.downcast::<CharacterData>().unwrap();
                        serializer.write_text(&cdata.data())?;
                    },

                    NodeTypeId::CharacterData(CharacterDataTypeId::Comment) => {
                        let cdata = n.downcast::<CharacterData>().unwrap();
                        serializer.write_comment(&cdata.data())?;
                    },

                    NodeTypeId::CharacterData(CharacterDataTypeId::ProcessingInstruction) => {
                        let pi = n.downcast::<ProcessingInstruction>().unwrap();
                        let data = pi.upcast::<CharacterData>().data();
                        serializer.write_processing_instruction(pi.target(), &data)?;
                    },

                    NodeTypeId::DocumentFragment(_) => {},

                    NodeTypeId::Document(_) => panic!("Can't serialize Document node itself"),
                    NodeTypeId::Element(_) => panic!("Element shouldn't appear here"),
                    NodeTypeId::Attr => panic!("Attr shouldn't appear here"),
                },
            }
        }

        Ok(())
    }
}
