use crate::lexer::{lex, Keyword, Operator, TextSpan, Token, TokenKind};
use crate::syntax::{Parse, ParseDiagnostic, SyntaxElement, SyntaxKind, SyntaxNode};

const MAX_RECURSION_DEPTH: usize = 128;

pub fn parse_source(source: &str) -> Parse {
    let tokens = lex(source);
    parse_lexed_source(source, &tokens)
}

/// Parses the complete token stream returned by `lex(source)`. Internal
/// callers that also retain lexical facts can share that stream without
/// tokenizing the same immutable source again.
pub(crate) fn parse_lexed_source(source: &str, tokens: &[Token]) -> Parse {
    let mut diagnostics = lexer_diagnostics(tokens);
    let mut parser = Parser {
        source,
        tokens,
        position: 0,
        diagnostics: Vec::new(),
        recursion_depth: 0,
        recursion_limit_recovered: false,
    };
    let root = parser.parse_source_file();
    diagnostics.extend(parser.diagnostics);

    Parse { root, diagnostics }
}

fn lexer_diagnostics(tokens: &[Token]) -> Vec<ParseDiagnostic> {
    tokens
        .iter()
        .filter(|token| token.kind.is_error())
        .map(|token| ParseDiagnostic {
            message: format!("Lexer error token: {:?}", token.kind),
            span: token.span,
        })
        .collect()
}

struct Parser<'source> {
    source: &'source str,
    tokens: &'source [Token],
    position: usize,
    diagnostics: Vec<ParseDiagnostic>,
    recursion_depth: usize,
    recursion_limit_recovered: bool,
}

impl Parser<'_> {
    fn parse_source_file(&mut self) -> SyntaxNode {
        let mut children = Vec::new();

        while !self.at(TokenKind::Eof) {
            if self.current().kind.is_trivia() {
                children.push(self.bump_token());
            } else if self.at(TokenKind::Hash) {
                children.push(self.parse_preprocessor_directive());
            } else {
                let start = self.position;
                children.push(self.parse_declaration_or_error(false));
                self.assert_progress(start, "source-file declaration");
            }
        }

        children.push(self.bump_token());
        SyntaxNode::new(SyntaxKind::SourceFile, children)
    }

    fn parse_declaration_or_error(&mut self, in_class: bool) -> SyntaxElement {
        self.with_recursion_budget(&[TokenKind::RightBrace], |parser| {
            parser.parse_declaration_or_error_inner(in_class)
        })
    }

    fn parse_declaration_or_error_inner(&mut self, in_class: bool) -> SyntaxElement {
        let mut prefix = Vec::new();
        self.collect_trivia(&mut prefix);
        self.collect_attributes(&mut prefix);
        self.collect_trivia(&mut prefix);
        self.collect_modifier_list(&mut prefix);
        self.collect_trivia(&mut prefix);

        let kind = self.current().kind;
        if self.at_keyword(Keyword::Class) {
            return self.parse_class_decl(prefix);
        }
        if self.at_keyword(Keyword::Enum) {
            return self.parse_enum_decl(prefix);
        }
        if self.at_keyword(Keyword::Typedef) {
            return self.parse_typedef_decl(prefix);
        }
        if self.at(TokenKind::Hash) {
            prefix.push(self.parse_preprocessor_directive());
            return node(SyntaxKind::PreprocessorDirective, prefix);
        }
        if self.at(TokenKind::Semicolon) {
            prefix.push(self.bump_token());
            return node(SyntaxKind::EmptyDecl, prefix);
        }
        if self.at(TokenKind::RightBrace) {
            self.error_here("Unexpected closing brace in declaration context");
            prefix.push(self.bump_token());
            return node(SyntaxKind::Error, prefix);
        }

        if self.looks_like_callable_decl() {
            self.parse_callable_decl(prefix, in_class)
        } else if is_declaration_start(kind) {
            self.parse_field_decl(prefix, in_class)
        } else {
            self.parse_error_until_sync(prefix)
        }
    }

    fn parse_class_decl(&mut self, mut children: Vec<SyntaxElement>) -> SyntaxElement {
        children.push(self.bump_token());
        self.collect_trivia(&mut children);

        if self.is_name_token() {
            children.push(self.bump_token());
        } else {
            self.error_here("Expected class name");
        }

        self.collect_trivia(&mut children);
        if self.at_operator(Operator::Less) {
            children.push(self.parse_angle_list(SyntaxKind::GenericArgList));
        }

        self.collect_trivia(&mut children);
        if self.at(TokenKind::Colon) || self.at_keyword(Keyword::Extends) {
            children.push(self.bump_token());
            self.collect_trivia(&mut children);
            children.push(self.parse_type_ref_until(&[TokenKind::LeftBrace, TokenKind::Semicolon]));
        }

        self.collect_trivia(&mut children);
        if self.at(TokenKind::LeftBrace) {
            children.push(self.parse_class_body());
        } else {
            self.error_here("Expected class body");
        }

        self.collect_trivia(&mut children);
        if self.at(TokenKind::Semicolon) {
            children.push(self.bump_token());
        }

        node(SyntaxKind::ClassDecl, children)
    }

    fn parse_enum_decl(&mut self, mut children: Vec<SyntaxElement>) -> SyntaxElement {
        children.push(self.bump_token());
        self.collect_trivia(&mut children);
        if self.is_name_token() {
            children.push(self.bump_token());
        }

        self.collect_trivia(&mut children);
        if self.at(TokenKind::Colon) {
            children.push(self.bump_token());
            self.collect_trivia(&mut children);
            children.push(self.parse_type_ref_until(&[TokenKind::LeftBrace, TokenKind::Semicolon]));
        }

        self.collect_trivia(&mut children);
        if self.at(TokenKind::LeftBrace) {
            children.push(self.bump_token());
            while !self.at(TokenKind::RightBrace) && !self.at(TokenKind::Eof) {
                if self.current().kind.is_trivia() {
                    children.push(self.bump_token());
                } else if self.is_name_token() {
                    children.push(self.parse_enum_member());
                } else {
                    children.push(self.bump_token());
                }
            }
            self.expect(
                TokenKind::RightBrace,
                &mut children,
                "Expected enum closing brace",
            );
        } else {
            self.error_here("Expected enum body");
        }

        self.collect_trivia(&mut children);
        if self.at(TokenKind::Semicolon) {
            children.push(self.bump_token());
        }

        node(SyntaxKind::EnumDecl, children)
    }

    fn parse_enum_member(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        children.push(self.bump_token());
        self.collect_trivia(&mut children);
        if self.at_operator(Operator::Equal) {
            children.push(self.bump_token());
            if !self.next_non_trivia_is_any(&[TokenKind::Comma, TokenKind::RightBrace]) {
                children.push(
                    self.parse_expression_until(&[TokenKind::Comma, TokenKind::RightBrace], 0),
                );
            }
        } else {
            while !matches!(
                self.current().kind,
                TokenKind::Comma | TokenKind::RightBrace | TokenKind::Eof
            ) {
                children.push(self.bump_token());
            }
        }
        self.collect_trivia(&mut children);
        if self.at(TokenKind::Comma) {
            children.push(self.bump_token());
        }
        node(SyntaxKind::EnumMember, children)
    }

    fn parse_typedef_decl(&mut self, mut children: Vec<SyntaxElement>) -> SyntaxElement {
        children.push(self.bump_token());
        while !matches!(
            self.current().kind,
            TokenKind::Semicolon | TokenKind::LeftBrace | TokenKind::RightBrace | TokenKind::Eof
        ) {
            if self.current().kind.is_trivia() {
                children.push(self.bump_token());
            } else {
                children.push(self.parse_type_ref_until(&[
                    TokenKind::Semicolon,
                    TokenKind::LeftBrace,
                    TokenKind::RightBrace,
                ]));
                break;
            }
        }
        self.expect(
            TokenKind::Semicolon,
            &mut children,
            "Expected typedef semicolon",
        );
        node(SyntaxKind::TypedefDecl, children)
    }

    fn parse_callable_decl(
        &mut self,
        mut children: Vec<SyntaxElement>,
        in_class: bool,
    ) -> SyntaxElement {
        while !matches!(
            self.current().kind,
            TokenKind::LeftParen | TokenKind::LeftBrace | TokenKind::Semicolon | TokenKind::Eof
        ) {
            children.push(self.bump_token());
        }

        if self.at(TokenKind::LeftParen) {
            children.push(self.parse_parameter_list());
        }

        self.collect_trivia(&mut children);
        if self.at(TokenKind::LeftBrace) {
            children.push(self.parse_statement_block());
            self.collect_trivia(&mut children);
            if self.at(TokenKind::Semicolon) {
                children.push(self.bump_token());
            }
        } else {
            self.expect(
                TokenKind::Semicolon,
                &mut children,
                "Expected callable semicolon or body",
            );
        }

        if in_class {
            node(SyntaxKind::MethodDecl, children)
        } else {
            node(SyntaxKind::FunctionDecl, children)
        }
    }

    fn parse_field_decl(
        &mut self,
        mut children: Vec<SyntaxElement>,
        allow_implicit_member_boundary: bool,
    ) -> SyntaxElement {
        let declaration_start = children.len();
        let mut hit_preprocessor_boundary = false;
        let mut hit_implicit_member_boundary = false;
        let mut paren_depth = 0usize;
        let mut bracket_depth = 0usize;
        let mut angle_depth = 0usize;
        let mut brace_depth = 0usize;

        while !self.at(TokenKind::Eof) {
            let kind = self.current().kind;
            let at_top_level =
                paren_depth == 0 && bracket_depth == 0 && angle_depth == 0 && brace_depth == 0;

            if at_top_level && matches!(kind, TokenKind::Semicolon | TokenKind::RightBrace) {
                break;
            }

            if allow_implicit_member_boundary
                && at_top_level
                && self.starts_new_member_after_unterminated_field(&children)
            {
                hit_implicit_member_boundary = true;
                break;
            }

            if allow_implicit_member_boundary
                && at_top_level
                && self.starts_attribute_member_after_unterminated_field(&children)
            {
                hit_implicit_member_boundary = true;
                break;
            }

            if at_top_level && self.at(TokenKind::Hash) {
                hit_preprocessor_boundary = true;
                break;
            }

            match kind {
                TokenKind::Operator(Operator::Equal) if at_top_level => {
                    children.push(self.bump_token());
                    if self.next_non_trivia_is(TokenKind::LeftBrace) {
                        self.collect_trivia(&mut children);
                        children.push(self.parse_initializer_expression());
                    } else if !self.next_non_trivia_is_any(&[
                        TokenKind::Comma,
                        TokenKind::Semicolon,
                        TokenKind::RightBrace,
                    ]) {
                        children.push(self.parse_expression_until(
                            &[
                                TokenKind::Comma,
                                TokenKind::Semicolon,
                                TokenKind::RightBrace,
                            ],
                            0,
                        ));
                    }
                    continue;
                }
                TokenKind::LeftParen => paren_depth += 1,
                TokenKind::RightParen => paren_depth = paren_depth.saturating_sub(1),
                TokenKind::LeftBracket => bracket_depth += 1,
                TokenKind::RightBracket => bracket_depth = bracket_depth.saturating_sub(1),
                TokenKind::LeftBrace => brace_depth += 1,
                TokenKind::RightBrace => brace_depth = brace_depth.saturating_sub(1),
                TokenKind::Operator(Operator::Less) => angle_depth += 1,
                TokenKind::Operator(Operator::Greater) => {
                    angle_depth = angle_depth.saturating_sub(1)
                }
                TokenKind::Operator(Operator::GreaterGreater) => {
                    angle_depth = angle_depth.saturating_sub(2)
                }
                _ => {}
            }

            children.push(self.bump_token());
        }

        if self.at(TokenKind::Semicolon) {
            children.push(self.bump_token());
        } else if !self.at(TokenKind::RightBrace)
            && !hit_preprocessor_boundary
            && !hit_implicit_member_boundary
        {
            self.error_here("Expected field semicolon");
        }
        if !hit_preprocessor_boundary {
            let declaration = children.split_off(declaration_start);
            children.extend(structure_declaration(declaration));
        }
        if hit_preprocessor_boundary {
            node(SyntaxKind::Error, children)
        } else {
            node(SyntaxKind::FieldDecl, children)
        }
    }

    fn parse_class_body(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        children.push(self.bump_token());

        while !self.at(TokenKind::RightBrace) && !self.at(TokenKind::Eof) {
            if self.current().kind.is_trivia() {
                children.push(self.bump_token());
            } else if self.at(TokenKind::Hash) {
                children.push(self.parse_preprocessor_directive());
            } else {
                let start = self.position;
                children.push(self.parse_declaration_or_error(true));
                self.assert_progress(start, "class-body declaration");
            }
        }

        self.expect(
            TokenKind::RightBrace,
            &mut children,
            "Expected class closing brace",
        );
        node(SyntaxKind::Block, children)
    }

    fn parse_statement_block(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        children.push(self.bump_token());

        while !self.at(TokenKind::RightBrace) && !self.at(TokenKind::Eof) {
            let start = self.position;
            children.push(self.parse_statement());
            self.assert_progress(start, "statement block item");
        }

        self.expect(
            TokenKind::RightBrace,
            &mut children,
            "Expected block closing brace",
        );
        node(SyntaxKind::Block, children)
    }

    fn parse_statement(&mut self) -> SyntaxElement {
        self.with_recursion_budget(&[TokenKind::Semicolon, TokenKind::RightBrace], |parser| {
            parser.parse_statement_inner()
        })
    }

    fn parse_statement_inner(&mut self) -> SyntaxElement {
        let mut prefix = Vec::new();
        self.collect_trivia(&mut prefix);

        if self.at(TokenKind::RightBrace) || self.at(TokenKind::Eof) {
            return node(SyntaxKind::EmptyStatement, prefix);
        }
        if self.at(TokenKind::Hash) {
            prefix.push(self.parse_preprocessor_directive());
            return node(SyntaxKind::PreprocessorDirective, prefix);
        }
        if self.at(TokenKind::LeftBrace) {
            if prefix.is_empty() {
                return self.parse_statement_block();
            }
            prefix.push(self.parse_statement_block());
            return node(SyntaxKind::Block, prefix);
        }
        if self.at(TokenKind::Semicolon) {
            prefix.push(self.bump_token());
            return node(SyntaxKind::EmptyStatement, prefix);
        }

        match self.current().kind {
            TokenKind::Keyword(Keyword::If) => self.parse_if_statement(prefix),
            TokenKind::Keyword(Keyword::For) => self.parse_for_statement(prefix),
            TokenKind::Keyword(Keyword::Foreach) => self.parse_foreach_statement(prefix),
            TokenKind::Keyword(Keyword::While) => self.parse_while_statement(prefix),
            TokenKind::Keyword(Keyword::Do) => self.parse_do_while_statement(prefix),
            TokenKind::Keyword(Keyword::Switch) => self.parse_switch_statement(prefix),
            TokenKind::Keyword(Keyword::Return) => self.parse_return_statement(prefix),
            TokenKind::Keyword(Keyword::Break) => {
                self.parse_flow_statement(prefix, SyntaxKind::BreakStatement)
            }
            TokenKind::Keyword(Keyword::Continue) => {
                self.parse_flow_statement(prefix, SyntaxKind::ContinueStatement)
            }
            TokenKind::Keyword(Keyword::Delete) => {
                self.parse_prefixed_expression_statement(prefix, SyntaxKind::DeleteStatement)
            }
            TokenKind::Keyword(Keyword::Thread) => {
                self.parse_prefixed_expression_statement(prefix, SyntaxKind::ThreadStatement)
            }
            _ if self.looks_like_local_decl_statement() => self.parse_local_decl_statement(prefix),
            _ => self.parse_expression_statement(prefix),
        }
    }

    fn parse_if_statement(&mut self, mut children: Vec<SyntaxElement>) -> SyntaxElement {
        children.push(self.bump_token());
        self.collect_trivia(&mut children);
        if self.at(TokenKind::LeftParen) {
            children.push(self.parse_parenthesized_expression_node(SyntaxKind::Condition));
        } else {
            self.error_here("Expected if condition");
        }
        if !self.at(TokenKind::RightBrace) && !self.at(TokenKind::Eof) {
            children.push(self.parse_statement());
        }
        self.collect_trivia(&mut children);
        if self.at_keyword(Keyword::Else) {
            let mut else_children = Vec::new();
            else_children.push(self.bump_token());
            if !self.at(TokenKind::RightBrace) && !self.at(TokenKind::Eof) {
                else_children.push(self.parse_statement());
            }
            children.push(node(SyntaxKind::ElseClause, else_children));
        }
        node(SyntaxKind::IfStatement, children)
    }

    fn parse_for_statement(&mut self, mut children: Vec<SyntaxElement>) -> SyntaxElement {
        children.push(self.bump_token());
        self.collect_trivia(&mut children);
        if self.at(TokenKind::LeftParen) {
            children.push(self.parse_for_header());
        } else {
            self.error_here("Expected for header");
        }
        if !self.at(TokenKind::RightBrace) && !self.at(TokenKind::Eof) {
            children.push(self.parse_statement());
        }
        node(SyntaxKind::ForStatement, children)
    }

    fn parse_foreach_statement(&mut self, mut children: Vec<SyntaxElement>) -> SyntaxElement {
        children.push(self.bump_token());
        self.collect_trivia(&mut children);
        if self.at(TokenKind::LeftParen) {
            children.push(self.parse_foreach_header());
        } else {
            self.error_here("Expected foreach header");
        }
        if !self.at(TokenKind::RightBrace) && !self.at(TokenKind::Eof) {
            children.push(self.parse_statement());
        }
        node(SyntaxKind::ForeachStatement, children)
    }

    fn parse_while_statement(&mut self, mut children: Vec<SyntaxElement>) -> SyntaxElement {
        children.push(self.bump_token());
        self.collect_trivia(&mut children);
        if self.at(TokenKind::LeftParen) {
            children.push(self.parse_parenthesized_expression_node(SyntaxKind::Condition));
        } else {
            self.error_here("Expected while condition");
        }
        if !self.at(TokenKind::RightBrace) && !self.at(TokenKind::Eof) {
            children.push(self.parse_statement());
        }
        node(SyntaxKind::WhileStatement, children)
    }

    fn parse_do_while_statement(&mut self, mut children: Vec<SyntaxElement>) -> SyntaxElement {
        children.push(self.bump_token());
        if !self.at(TokenKind::Eof) {
            children.push(self.parse_statement());
        }
        self.collect_trivia(&mut children);
        if self.at_keyword(Keyword::While) {
            children.push(self.bump_token());
            self.collect_trivia(&mut children);
            if self.at(TokenKind::LeftParen) {
                children.push(self.parse_parenthesized_expression_node(SyntaxKind::Condition));
            } else {
                self.error_here("Expected do-while condition");
            }
            self.collect_trivia(&mut children);
            if self.at(TokenKind::Semicolon) {
                children.push(self.bump_token());
            }
        } else {
            self.error_here("Expected while after do body");
        }
        node(SyntaxKind::DoWhileStatement, children)
    }

    fn parse_switch_statement(&mut self, mut children: Vec<SyntaxElement>) -> SyntaxElement {
        children.push(self.bump_token());
        self.collect_trivia(&mut children);
        if self.at(TokenKind::LeftParen) {
            children.push(self.parse_parenthesized_expression_node(SyntaxKind::SwitchHeader));
        } else {
            self.error_here("Expected switch header");
        }
        self.collect_trivia(&mut children);
        if self.at(TokenKind::LeftBrace) {
            children.push(self.bump_token());
            while !self.at(TokenKind::RightBrace) && !self.at(TokenKind::Eof) {
                if self.current().kind.is_trivia() {
                    children.push(self.bump_token());
                } else if self.at_keyword(Keyword::Case) {
                    let start = self.position;
                    children.push(self.parse_switch_section());
                    self.assert_progress(start, "switch section");
                } else if self.at_keyword(Keyword::Default) {
                    let start = self.position;
                    children.push(self.parse_switch_section());
                    self.assert_progress(start, "switch section");
                } else {
                    let start = self.position;
                    children.push(self.parse_statement());
                    self.assert_progress(start, "switch statement item");
                }
            }
            self.expect(
                TokenKind::RightBrace,
                &mut children,
                "Expected switch closing brace",
            );
        } else {
            self.error_here("Expected switch body");
        }
        node(SyntaxKind::SwitchStatement, children)
    }

    fn parse_switch_section(&mut self) -> SyntaxElement {
        let mut children = Vec::new();

        while self.at_keyword(Keyword::Case) || self.at_keyword(Keyword::Default) {
            if self.at_keyword(Keyword::Case) {
                let start = self.position;
                children.push(self.parse_case_clause());
                self.assert_progress(start, "switch case clause");
            } else {
                let start = self.position;
                children.push(self.parse_default_clause());
                self.assert_progress(start, "switch default clause");
            }
            self.collect_trivia(&mut children);
        }

        while !self.at(TokenKind::RightBrace)
            && !self.at(TokenKind::Eof)
            && !self.at_keyword(Keyword::Case)
            && !self.at_keyword(Keyword::Default)
        {
            if self.current().kind.is_trivia() {
                children.push(self.bump_token());
            } else {
                let start = self.position;
                children.push(self.parse_statement());
                self.assert_progress(start, "switch section statement");
            }
        }

        node(SyntaxKind::SwitchSection, children)
    }

    fn parse_case_clause(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        children.push(self.bump_token());
        self.collect_trivia(&mut children);
        if !self.at(TokenKind::Colon) {
            children.push(self.parse_expression_until(&[TokenKind::Colon], 0));
            self.collect_trivia(&mut children);
        }
        self.expect(TokenKind::Colon, &mut children, "Expected case colon");
        node(SyntaxKind::CaseClause, children)
    }

    fn parse_default_clause(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        children.push(self.bump_token());
        self.collect_trivia(&mut children);
        self.expect(TokenKind::Colon, &mut children, "Expected default colon");
        node(SyntaxKind::DefaultClause, children)
    }

    fn parse_return_statement(&mut self, mut children: Vec<SyntaxElement>) -> SyntaxElement {
        children.push(self.bump_token());
        if !self.next_non_trivia_is_any(&[TokenKind::Semicolon, TokenKind::RightBrace]) {
            children.push(
                self.parse_expression_until(&[TokenKind::Semicolon, TokenKind::RightBrace], 0),
            );
        }
        if self.at(TokenKind::Semicolon) {
            children.push(self.bump_token());
        }
        node(SyntaxKind::ReturnStatement, children)
    }

    fn parse_flow_statement(
        &mut self,
        mut children: Vec<SyntaxElement>,
        kind: SyntaxKind,
    ) -> SyntaxElement {
        children.push(self.bump_token());
        self.collect_trivia(&mut children);
        if self.at(TokenKind::Semicolon) {
            children.push(self.bump_token());
        }
        node(kind, children)
    }

    fn parse_prefixed_expression_statement(
        &mut self,
        mut children: Vec<SyntaxElement>,
        kind: SyntaxKind,
    ) -> SyntaxElement {
        children.push(self.bump_token());
        if !self.next_non_trivia_is_any(&[TokenKind::Semicolon, TokenKind::RightBrace]) {
            children.push(
                self.parse_expression_until(&[TokenKind::Semicolon, TokenKind::RightBrace], 0),
            );
        }
        if self.at(TokenKind::Semicolon) {
            children.push(self.bump_token());
        }
        node(kind, children)
    }

    fn parse_expression_statement(&mut self, mut children: Vec<SyntaxElement>) -> SyntaxElement {
        children
            .push(self.parse_expression_until(&[TokenKind::Semicolon, TokenKind::RightBrace], 0));
        if self.at(TokenKind::Semicolon) {
            children.push(self.bump_token());
        }
        node(SyntaxKind::ExpressionStatement, children)
    }

    fn parse_local_decl_statement(&mut self, mut children: Vec<SyntaxElement>) -> SyntaxElement {
        self.parse_local_decl_statement_until(
            &mut children,
            &[TokenKind::Semicolon, TokenKind::RightBrace],
        );
        if self.at(TokenKind::Semicolon) {
            children.push(self.bump_token());
        }
        children = structure_declaration(children);
        node(SyntaxKind::LocalDeclStatement, children)
    }

    fn parse_local_decl_statement_until(
        &mut self,
        children: &mut Vec<SyntaxElement>,
        stop: &[TokenKind],
    ) {
        let mut paren_depth = 0usize;
        let mut bracket_depth = 0usize;
        let mut angle_depth = 0usize;
        while !self.at(TokenKind::Eof) && !stop.contains(&self.current().kind) {
            let at_top_level = paren_depth == 0 && bracket_depth == 0 && angle_depth == 0;
            if at_top_level && self.starts_new_statement_after_unterminated_local(children) {
                break;
            }

            if self.current().kind.is_trivia() || self.at(TokenKind::Comma) {
                children.push(self.bump_token());
            } else if self.at(TokenKind::LeftBrace) {
                children.push(self.parse_initializer_expression());
            } else if self.at_operator(Operator::Equal) {
                let assignment = self.current();
                children.push(self.bump_token());
                if self.next_line_starts_local_decl_statement() {
                    self.error_at_span(
                        "Expected expression",
                        TextSpan::new(assignment.span.end, assignment.span.end),
                    );
                    break;
                }
                children.push(self.parse_expression_until(&local_decl_expression_stops(stop), 0));
            } else {
                match self.current().kind {
                    TokenKind::LeftParen => paren_depth += 1,
                    TokenKind::RightParen => paren_depth = paren_depth.saturating_sub(1),
                    TokenKind::LeftBracket => bracket_depth += 1,
                    TokenKind::RightBracket => bracket_depth = bracket_depth.saturating_sub(1),
                    TokenKind::Operator(Operator::Less) => angle_depth += 1,
                    TokenKind::Operator(Operator::Greater) => {
                        angle_depth = angle_depth.saturating_sub(1)
                    }
                    TokenKind::Operator(Operator::GreaterGreater) => {
                        angle_depth = angle_depth.saturating_sub(2)
                    }
                    _ => {}
                }
                children.push(self.bump_token());
            }
        }
    }

    fn parse_for_header(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        children.push(self.bump_token());

        if !self.next_non_trivia_is_any(&[TokenKind::Semicolon, TokenKind::RightParen]) {
            children.push(self.parse_for_initializer());
        }
        self.collect_trivia(&mut children);
        if self.at(TokenKind::Semicolon) {
            children.push(self.bump_token());
        } else if !self.at(TokenKind::RightParen) && !self.at(TokenKind::Eof) {
            self.error_here("Expected for initializer semicolon");
        }

        if !self.next_non_trivia_is_any(&[TokenKind::Semicolon, TokenKind::RightParen]) {
            children.push(self.parse_for_condition());
        }
        self.collect_trivia(&mut children);
        if self.at(TokenKind::Semicolon) {
            children.push(self.bump_token());
        } else if !self.at(TokenKind::RightParen) && !self.at(TokenKind::Eof) {
            self.error_here("Expected for condition semicolon");
        }

        if !self.next_non_trivia_is_any(&[TokenKind::RightParen]) {
            children.push(self.parse_for_increment());
        }
        self.collect_trivia(&mut children);
        self.expect(
            TokenKind::RightParen,
            &mut children,
            "Expected for header closing paren",
        );
        node(SyntaxKind::ForHeader, children)
    }

    fn parse_for_initializer(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        self.collect_trivia(&mut children);
        if self.looks_like_local_decl_statement_in_for_header() {
            let mut declaration_children = Vec::new();
            self.parse_local_decl_statement_until(
                &mut declaration_children,
                &[TokenKind::Semicolon],
            );
            children.push(node(
                SyntaxKind::LocalDeclStatement,
                structure_declaration(declaration_children),
            ));
        } else {
            self.parse_for_expression_list(
                &mut children,
                &[TokenKind::Semicolon, TokenKind::RightParen],
            );
        }
        node(SyntaxKind::ForInitializer, children)
    }

    fn parse_for_condition(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        children
            .push(self.parse_expression_until(&[TokenKind::Semicolon, TokenKind::RightParen], 0));
        node(SyntaxKind::ForCondition, children)
    }

    fn parse_for_increment(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        self.parse_for_expression_list(&mut children, &[TokenKind::RightParen]);
        node(SyntaxKind::ForIncrement, children)
    }

    fn parse_for_expression_list(&mut self, children: &mut Vec<SyntaxElement>, stop: &[TokenKind]) {
        while !self.at(TokenKind::Eof) && !stop.contains(&self.current().kind) {
            if self.current().kind.is_trivia() || self.at(TokenKind::Comma) {
                children.push(self.bump_token());
            } else {
                let mut expression_stops = vec![TokenKind::Comma];
                expression_stops.extend_from_slice(stop);
                let start = self.position;
                children.push(self.parse_expression_until(&expression_stops, 0));
                self.assert_progress(start, "for expression list item");
            }
        }
    }

    fn parse_foreach_header(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        children.push(self.bump_token());
        if !self.next_non_trivia_is_any(&[TokenKind::Colon, TokenKind::RightParen]) {
            children.push(self.parse_foreach_variable_list());
        }
        self.collect_trivia(&mut children);
        if self.at(TokenKind::Colon) {
            children.push(self.bump_token());
        } else if !self.at(TokenKind::RightParen) && !self.at(TokenKind::Eof) {
            self.error_here("Expected foreach header colon");
        }
        if !self.next_non_trivia_is_any(&[TokenKind::RightParen]) {
            children.push(self.parse_foreach_iterable());
        }
        self.collect_trivia(&mut children);
        self.expect(
            TokenKind::RightParen,
            &mut children,
            "Expected foreach header closing paren",
        );
        node(SyntaxKind::ForeachHeader, children)
    }

    fn parse_foreach_variable_list(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        while !self.at(TokenKind::Colon)
            && !self.at(TokenKind::RightParen)
            && !self.at(TokenKind::Eof)
        {
            if self.current().kind.is_trivia() || self.at(TokenKind::Comma) {
                children.push(self.bump_token());
            } else {
                let start = self.position;
                children.push(self.parse_foreach_variable());
                self.assert_progress(start, "foreach variable");
            }
        }
        node(SyntaxKind::ForeachVariableList, children)
    }

    fn parse_foreach_variable(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        let mut paren_depth = 0usize;
        let mut bracket_depth = 0usize;
        let mut angle_depth = 0usize;

        while !self.at(TokenKind::Eof) {
            let kind = self.current().kind;
            let at_top_level = paren_depth == 0 && bracket_depth == 0 && angle_depth == 0;
            if at_top_level
                && matches!(
                    kind,
                    TokenKind::Comma | TokenKind::Colon | TokenKind::RightParen
                )
            {
                break;
            }

            match kind {
                TokenKind::LeftParen => paren_depth += 1,
                TokenKind::RightParen => paren_depth = paren_depth.saturating_sub(1),
                TokenKind::LeftBracket => bracket_depth += 1,
                TokenKind::RightBracket => bracket_depth = bracket_depth.saturating_sub(1),
                TokenKind::Operator(Operator::Less) => angle_depth += 1,
                TokenKind::Operator(Operator::Greater) => {
                    angle_depth = angle_depth.saturating_sub(1)
                }
                TokenKind::Operator(Operator::GreaterGreater) => {
                    angle_depth = angle_depth.saturating_sub(2)
                }
                _ => {}
            }

            children.push(self.bump_token());
        }
        node(
            SyntaxKind::ForeachVariable,
            structure_foreach_variable(children),
        )
    }

    fn parse_foreach_iterable(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        children.push(self.parse_expression_until(&[TokenKind::RightParen], 0));
        node(SyntaxKind::ForeachIterable, children)
    }

    fn parse_parenthesized_expression_node(&mut self, kind: SyntaxKind) -> SyntaxElement {
        let mut children = Vec::new();
        children.push(self.bump_token());
        if !self.next_non_trivia_is_any(&[TokenKind::RightParen]) {
            children.push(self.parse_expression_until(&[TokenKind::RightParen], 0));
        }
        self.collect_trivia(&mut children);
        self.expect(
            TokenKind::RightParen,
            &mut children,
            "Expected parenthesized expression closing paren",
        );
        node(kind, children)
    }

    fn parse_expression_until(&mut self, stop: &[TokenKind], min_bp: u8) -> SyntaxElement {
        self.parse_expression_bp(stop, min_bp)
    }

    fn parse_expression_bp(&mut self, stop: &[TokenKind], min_bp: u8) -> SyntaxElement {
        self.with_recursion_budget(stop, |parser| {
            parser.parse_expression_bp_inner(stop, min_bp)
        })
    }

    fn parse_expression_bp_inner(&mut self, stop: &[TokenKind], min_bp: u8) -> SyntaxElement {
        let mut lhs = self.parse_prefix_expression(stop);

        loop {
            if self.at(TokenKind::Eof) || self.next_non_trivia_is_any(stop) {
                break;
            }

            if self.next_non_trivia_is(TokenKind::LeftParen) {
                let mut children = vec![lhs];
                self.collect_trivia(&mut children);
                children.push(self.parse_argument_list());
                lhs = node(SyntaxKind::CallExpression, children);
                continue;
            }

            if self.next_non_trivia_is(TokenKind::Dot) {
                let mut children = vec![lhs];
                self.collect_trivia(&mut children);
                children.push(self.bump_token());
                self.collect_trivia(&mut children);
                if self.is_name_token() {
                    children.push(self.parse_name_expression());
                } else {
                    self.error_here("Expected member name");
                }
                lhs = node(SyntaxKind::MemberAccessExpression, children);
                continue;
            }

            if self.next_non_trivia_is(TokenKind::LeftBracket) {
                let mut children = vec![lhs];
                self.collect_trivia(&mut children);
                children.push(self.bump_token());
                if !self.next_non_trivia_is(TokenKind::RightBracket) {
                    children.push(self.parse_expression_bp(&[TokenKind::RightBracket], 0));
                }
                self.collect_trivia(&mut children);
                self.expect(
                    TokenKind::RightBracket,
                    &mut children,
                    "Expected index expression closing bracket",
                );
                lhs = node(SyntaxKind::IndexExpression, children);
                continue;
            }

            if self.next_non_trivia_is_operator(Operator::PlusPlus)
                || self.next_non_trivia_is_operator(Operator::MinusMinus)
            {
                let mut children = vec![lhs];
                self.collect_trivia(&mut children);
                children.push(self.bump_token());
                lhs = node(SyntaxKind::PostfixExpression, children);
                continue;
            }

            if self.next_non_trivia_is(TokenKind::Question) {
                if min_bp > 1 {
                    break;
                }
                let mut children = vec![lhs];
                self.collect_trivia(&mut children);
                children.push(self.bump_token());
                children.push(self.parse_expression_bp(&[TokenKind::Colon], 0));
                self.collect_trivia(&mut children);
                self.expect(TokenKind::Colon, &mut children, "Expected ternary colon");
                children.push(self.parse_expression_bp(stop, 1));
                lhs = node(SyntaxKind::TernaryExpression, children);
                continue;
            }

            let Some((left_bp, right_bp, kind)) = self.current_binary_binding_power() else {
                break;
            };
            if left_bp < min_bp {
                break;
            }

            let mut children = vec![lhs];
            self.collect_trivia(&mut children);
            children.push(self.bump_token());
            children.push(self.parse_expression_bp(stop, right_bp));
            lhs = node(kind, children);
        }

        lhs
    }

    fn parse_prefix_expression(&mut self, stop: &[TokenKind]) -> SyntaxElement {
        let mut children = Vec::new();
        self.collect_trivia(&mut children);

        if self.at(TokenKind::Eof) || self.next_non_trivia_is_any(stop) {
            self.error_here("Expected expression");
            return node(SyntaxKind::Error, children);
        }

        if self.at(TokenKind::LeftBrace) {
            children.push(self.parse_initializer_expression());
            return single_or_wrapped_expression(children);
        }

        if self.at(TokenKind::LeftParen) {
            children.push(self.bump_token());
            if !self.next_non_trivia_is(TokenKind::RightParen) {
                children.push(self.parse_expression_bp(&[TokenKind::RightParen], 0));
            }
            self.collect_trivia(&mut children);
            self.expect(
                TokenKind::RightParen,
                &mut children,
                "Expected parenthesized expression closing paren",
            );
            if self.next_token_can_start_expression() {
                children.push(self.parse_expression_bp(stop, 14));
                return node(SyntaxKind::CastExpression, children);
            }
            return node(SyntaxKind::ParenthesizedExpression, children);
        }

        if self.at_keyword(Keyword::New) {
            children.push(self.bump_token());
            self.collect_trivia(&mut children);
            if self.is_name_token() {
                children.push(self.parse_type_name_expression());
            }
            if self.next_non_trivia_is(TokenKind::LeftParen) {
                self.collect_trivia(&mut children);
                children.push(self.parse_argument_list());
            }
            return node(SyntaxKind::NewExpression, children);
        }

        if self.at_prefix_operator() {
            children.push(self.bump_token());
            children.push(self.parse_expression_bp(stop, 14));
            return node(SyntaxKind::UnaryExpression, children);
        }

        if self.is_name_token() {
            children.push(self.parse_name_expression());
            return single_or_wrapped_expression(children);
        }

        if matches!(self.current().kind, TokenKind::Number | TokenKind::String) {
            children.push(self.bump_token());
            return node(SyntaxKind::LiteralExpression, children);
        }

        children.push(self.bump_token());
        node(SyntaxKind::Expression, children)
    }

    fn parse_name_expression(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        children.push(self.bump_token());
        if self.next_non_trivia_is_operator(Operator::Less)
            && self.looks_like_generic_argument_list()
        {
            self.collect_trivia(&mut children);
            children.push(self.parse_angle_list(SyntaxKind::GenericArgList));
        }
        node(SyntaxKind::NameExpression, children)
    }

    fn parse_type_name_expression(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        children.push(self.bump_token());
        if self.next_non_trivia_is_operator(Operator::Less) {
            self.collect_trivia(&mut children);
            children.push(self.parse_angle_list(SyntaxKind::GenericArgList));
        }
        node(SyntaxKind::NameExpression, children)
    }

    fn parse_argument_list(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        children.push(self.bump_token());

        while !self.at(TokenKind::RightParen) && !self.at(TokenKind::Eof) {
            if self.current().kind.is_trivia() || self.at(TokenKind::Comma) {
                children.push(self.bump_token());
            } else {
                let start = self.position;
                children.push(self.parse_argument());
                self.assert_progress(start, "argument-list item");
            }
        }

        self.expect(
            TokenKind::RightParen,
            &mut children,
            "Expected argument-list closing paren",
        );
        node(SyntaxKind::ArgumentList, children)
    }

    fn parse_argument(&mut self) -> SyntaxElement {
        if self.is_name_token() && self.next_significant_kind(1) == Some(TokenKind::Colon) {
            let mut children = Vec::new();
            children.push(self.parse_name_expression());
            self.collect_trivia(&mut children);
            children.push(self.bump_token());
            children.push(self.parse_expression_bp(&[TokenKind::Comma, TokenKind::RightParen], 0));
            return node(SyntaxKind::NamedArgument, children);
        }

        self.parse_expression_bp(&[TokenKind::Comma, TokenKind::RightParen], 0)
    }

    fn parse_initializer_expression(&mut self) -> SyntaxElement {
        self.with_recursion_budget(&[TokenKind::RightBrace], |parser| {
            parser.parse_initializer_expression_inner()
        })
    }

    fn parse_initializer_expression_inner(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        children.push(self.bump_token());
        while !self.at(TokenKind::RightBrace) && !self.at(TokenKind::Eof) {
            if self.current().kind.is_trivia() || self.at(TokenKind::Comma) {
                children.push(self.bump_token());
            } else if self.at(TokenKind::LeftBrace) {
                children.push(self.parse_initializer_expression());
            } else {
                let start = self.position;
                children
                    .push(self.parse_expression_bp(&[TokenKind::Comma, TokenKind::RightBrace], 0));
                self.assert_progress(start, "initializer expression item");
            }
        }
        self.expect(
            TokenKind::RightBrace,
            &mut children,
            "Expected initializer expression closing brace",
        );
        node(SyntaxKind::InitializerExpression, children)
    }

    fn parse_parameter_list(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        children.push(self.bump_token());

        while !self.at(TokenKind::RightParen) && !self.at(TokenKind::Eof) {
            if self.current().kind.is_trivia() || self.at(TokenKind::Comma) {
                children.push(self.bump_token());
            } else {
                let start = self.position;
                children.push(self.parse_parameter());
                self.assert_progress(start, "parameter-list item");
            }
        }

        self.expect(
            TokenKind::RightParen,
            &mut children,
            "Expected parameter-list closing paren",
        );
        node(SyntaxKind::ParameterList, children)
    }

    fn parse_parameter(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        let mut paren_depth = 0usize;
        let mut bracket_depth = 0usize;
        let mut angle_depth = 0usize;
        let mut brace_depth = 0usize;

        while !self.at(TokenKind::Eof) {
            let kind = self.current().kind;
            let at_top_level =
                paren_depth == 0 && bracket_depth == 0 && angle_depth == 0 && brace_depth == 0;
            if paren_depth == 0
                && bracket_depth == 0
                && angle_depth == 0
                && brace_depth == 0
                && matches!(kind, TokenKind::Comma | TokenKind::RightParen)
            {
                break;
            }

            if at_top_level && kind == TokenKind::Operator(Operator::Equal) {
                children.push(self.bump_token());
                if !self.next_non_trivia_is_any(&[TokenKind::Comma, TokenKind::RightParen]) {
                    children.push(
                        self.parse_expression_until(&[TokenKind::Comma, TokenKind::RightParen], 0),
                    );
                }
                continue;
            }

            match kind {
                TokenKind::LeftParen => paren_depth += 1,
                TokenKind::RightParen => paren_depth = paren_depth.saturating_sub(1),
                TokenKind::LeftBracket => bracket_depth += 1,
                TokenKind::RightBracket => bracket_depth = bracket_depth.saturating_sub(1),
                TokenKind::LeftBrace => brace_depth += 1,
                TokenKind::RightBrace => brace_depth = brace_depth.saturating_sub(1),
                TokenKind::Operator(Operator::Less) => angle_depth += 1,
                TokenKind::Operator(Operator::Greater) => {
                    angle_depth = angle_depth.saturating_sub(1)
                }
                TokenKind::Operator(Operator::GreaterGreater) => {
                    angle_depth = angle_depth.saturating_sub(2)
                }
                _ => {}
            }

            children.push(self.bump_token());
        }
        node(SyntaxKind::Parameter, children)
    }

    fn parse_angle_list(&mut self, kind: SyntaxKind) -> SyntaxElement {
        let mut children = Vec::new();
        let mut depth = 0usize;

        while !self.at(TokenKind::Eof) {
            if self.at_operator(Operator::Less) {
                depth += 1;
                children.push(self.bump_token());
                continue;
            }

            if self.at_operator(Operator::Greater) {
                children.push(self.bump_token());
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return node(kind, children);
                }
                continue;
            }

            if self.at_operator(Operator::GreaterGreater) {
                children.push(self.bump_token());
                depth = depth.saturating_sub(2);
                if depth == 0 {
                    return node(kind, children);
                }
                continue;
            }

            if depth == 0 {
                break;
            }

            children.push(self.bump_token());
        }

        self.error_at_span("Expected generic closing angle", children_span(&children));
        node(kind, children)
    }

    fn parse_type_ref_until(&mut self, stop: &[TokenKind]) -> SyntaxElement {
        let mut children = Vec::new();
        let mut paren_depth = 0usize;
        let mut bracket_depth = 0usize;
        let mut angle_depth = 0usize;

        while !self.at(TokenKind::Eof) {
            let kind = self.current().kind;
            if paren_depth == 0 && bracket_depth == 0 && angle_depth == 0 && stop.contains(&kind) {
                break;
            }

            match kind {
                TokenKind::LeftParen => paren_depth += 1,
                TokenKind::RightParen => paren_depth = paren_depth.saturating_sub(1),
                TokenKind::LeftBracket => bracket_depth += 1,
                TokenKind::RightBracket => bracket_depth = bracket_depth.saturating_sub(1),
                TokenKind::Operator(Operator::Less) => angle_depth += 1,
                TokenKind::Operator(Operator::Greater) => {
                    angle_depth = angle_depth.saturating_sub(1)
                }
                TokenKind::Operator(Operator::GreaterGreater) => {
                    angle_depth = angle_depth.saturating_sub(2)
                }
                _ => {}
            }

            children.push(self.bump_token());
        }

        node(SyntaxKind::TypeRef, children)
    }

    fn parse_preprocessor_directive(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        children.push(self.bump_token());

        while !self.at(TokenKind::Eof) {
            let token = self.current();
            children.push(self.bump_token());
            if self.token_ends_physical_line(token) {
                break;
            }
        }

        node(SyntaxKind::PreprocessorDirective, children)
    }

    fn parse_error_until_sync(&mut self, mut children: Vec<SyntaxElement>) -> SyntaxElement {
        self.error_here("Unexpected token in declaration context");
        if self.at(TokenKind::LeftBrace) {
            children.push(self.parse_statement_block());
            return node(SyntaxKind::Error, children);
        }
        while !matches!(
            self.current().kind,
            TokenKind::Semicolon | TokenKind::LeftBrace | TokenKind::RightBrace | TokenKind::Eof
        ) {
            if !children.is_empty() && self.at_declaration_recovery_sync_start() {
                break;
            }
            children.push(self.bump_token());
        }
        if self.at(TokenKind::Semicolon) {
            children.push(self.bump_token());
        }
        node(SyntaxKind::Error, children)
    }

    fn at_declaration_recovery_sync_start(&self) -> bool {
        if self.at(TokenKind::LeftBracket) {
            return true;
        }

        match self.current().kind {
            TokenKind::Keyword(keyword) => is_declaration_recovery_sync_keyword(keyword),
            _ => is_declaration_sync_start(self.current().kind),
        }
    }

    fn collect_attributes(&mut self, children: &mut Vec<SyntaxElement>) {
        loop {
            self.collect_trivia(children);
            if !self.at(TokenKind::LeftBracket) {
                break;
            }
            children.push(self.parse_attribute_list());
            self.collect_trivia(children);
            if self.at(TokenKind::Semicolon) {
                children.push(self.bump_token());
            }
        }
    }

    fn parse_attribute_list(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        children.push(self.bump_token());

        while !self.at(TokenKind::RightBracket) && !self.at(TokenKind::Eof) {
            if self.is_name_token() {
                children.push(self.parse_attribute());
            } else {
                children.push(self.bump_token());
            }
        }

        self.expect(
            TokenKind::RightBracket,
            &mut children,
            "Expected attribute-list closing bracket",
        );
        node(SyntaxKind::AttributeList, children)
    }

    fn parse_attribute(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        children.push(self.bump_token());
        while !matches!(
            self.current().kind,
            TokenKind::Comma | TokenKind::RightBracket | TokenKind::Eof
        ) {
            if self.at(TokenKind::LeftParen) {
                children.push(self.parse_attribute_args());
            } else {
                children.push(self.bump_token());
            }
        }
        if self.at(TokenKind::Comma) {
            children.push(self.bump_token());
        }
        node(SyntaxKind::Attribute, children)
    }

    fn parse_attribute_args(&mut self) -> SyntaxElement {
        let mut children = Vec::new();
        children.push(self.bump_token());

        while !self.at(TokenKind::RightParen) && !self.at(TokenKind::Eof) {
            if self.current().kind.is_trivia() || self.at(TokenKind::Comma) {
                children.push(self.bump_token());
            } else {
                children.push(self.parse_argument());
            }
        }

        self.expect(
            TokenKind::RightParen,
            &mut children,
            "Expected attribute argument-list closing paren",
        );
        node(SyntaxKind::AttributeArgs, children)
    }

    fn collect_modifier_list(&mut self, children: &mut Vec<SyntaxElement>) {
        if !self.at_modifier() {
            return;
        }

        let mut modifiers = Vec::new();
        while self.at_modifier() || self.current().kind.is_trivia() {
            if self.current().kind.is_trivia() {
                modifiers.push(self.bump_token());
            } else {
                modifiers.push(self.bump_token());
            }
        }
        children.push(node(SyntaxKind::ModifierList, modifiers));
    }

    fn collect_trivia(&mut self, children: &mut Vec<SyntaxElement>) {
        while self.current().kind.is_trivia() {
            children.push(self.bump_token());
        }
    }

    fn looks_like_callable_decl(&self) -> bool {
        let mut index = self.position;
        let mut name_like_count = 0usize;
        while index < self.tokens.len() {
            let token = self.tokens[index];
            if token.kind.is_trivia() {
                if name_like_count >= 2
                    && self.token_text(token).contains('\n')
                    && self
                        .next_non_trivia_kind_after(index + 1)
                        .is_some_and(is_declaration_or_modifier_start)
                {
                    return false;
                }
                index += 1;
                continue;
            }

            match token.kind {
                TokenKind::LeftParen => return true,
                TokenKind::Operator(Operator::Equal) => return false,
                TokenKind::Semicolon
                | TokenKind::LeftBrace
                | TokenKind::LeftBracket
                | TokenKind::RightBracket
                | TokenKind::RightBrace
                | TokenKind::Eof => return false,
                TokenKind::Identifier | TokenKind::Keyword(_) => {
                    name_like_count += 1;
                    index += 1;
                }
                _ => index += 1,
            }
        }
        false
    }

    fn starts_new_member_after_unterminated_field(&self, children: &[SyntaxElement]) -> bool {
        if children.is_empty() || !is_declaration_or_modifier_start(self.current().kind) {
            return false;
        }

        if !partial_declaration_has_declarator(children) {
            return false;
        }

        let mut saw_trailing_newline = false;
        for child in children.iter().rev() {
            match child {
                SyntaxElement::Token(token) if token.kind.is_trivia() => {
                    saw_trailing_newline |= self.token_text(*token).contains('\n');
                }
                SyntaxElement::Token(_) | SyntaxElement::Node(_) => return saw_trailing_newline,
            }
        }

        false
    }

    fn starts_attribute_member_after_unterminated_field(&self, children: &[SyntaxElement]) -> bool {
        if children.is_empty() || !self.at(TokenKind::LeftBracket) {
            return false;
        }

        if !partial_declaration_has_declarator(children) {
            return false;
        }

        if !self
            .next_non_trivia_kind_after(self.position + 1)
            .is_some_and(|kind| matches!(kind, TokenKind::Identifier | TokenKind::Keyword(_)))
        {
            return false;
        }

        let mut saw_trailing_newline = false;
        for child in children.iter().rev() {
            match child {
                SyntaxElement::Token(token) if token.kind.is_trivia() => {
                    saw_trailing_newline |= self.token_text(*token).contains('\n');
                }
                SyntaxElement::Token(_) | SyntaxElement::Node(_) => return saw_trailing_newline,
            }
        }

        false
    }

    fn starts_new_statement_after_unterminated_local(&self, children: &[SyntaxElement]) -> bool {
        if children.is_empty() || !self.current_token_can_start_statement_after_local() {
            return false;
        }
        if !partial_declaration_has_declarator(children) {
            return false;
        }

        let mut saw_newline = false;
        for child in children.iter().rev() {
            match child {
                SyntaxElement::Token(token) if token.kind.is_trivia() => {
                    if self.token_text(*token).contains('\n') {
                        saw_newline = true;
                    }
                }
                SyntaxElement::Token(token) => {
                    return saw_newline
                        && !matches!(
                            token.kind,
                            TokenKind::Comma | TokenKind::Operator(Operator::Equal)
                        );
                }
                SyntaxElement::Node(_) => return saw_newline,
            }
        }

        false
    }

    fn current_token_can_start_statement_after_local(&self) -> bool {
        token_kind_can_start_statement_after_local(self.current().kind)
    }

    fn looks_like_local_decl_statement(&self) -> bool {
        self.looks_like_local_decl_statement_from(self.position)
    }

    fn next_line_starts_local_decl_statement(&self) -> bool {
        let mut index = self.position;
        let mut saw_newline = false;
        while let Some(token) = self.tokens.get(index) {
            if !token.kind.is_trivia() {
                break;
            }
            saw_newline |= self.token_text(*token).contains('\n');
            index += 1;
        }

        saw_newline && self.looks_like_local_decl_statement_from(index)
    }

    fn looks_like_local_decl_statement_from(&self, start: usize) -> bool {
        let Some(start_token) = self.tokens.get(start) else {
            return false;
        };
        if !is_declaration_start(start_token.kind)
            || start_token.kind == TokenKind::Keyword(Keyword::New)
        {
            return false;
        }

        let mut index = start;
        let mut saw_name_after_type = false;
        let mut saw_equal = false;
        let mut saw_newline_after_name = false;
        let mut saw_newline_before_name = false;
        let mut paren_depth = 0usize;
        let mut bracket_depth = 0usize;
        let mut brace_depth = 0usize;
        let mut angle_depth = 0usize;

        while index < self.tokens.len() {
            let token = self.tokens[index];
            let kind = token.kind;
            let at_top_level =
                paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0;

            if at_top_level && saw_name_after_type && kind.is_trivia() {
                saw_newline_after_name |= self.token_text(token).contains('\n');
                index += 1;
                continue;
            }

            if at_top_level && !saw_name_after_type && kind.is_trivia() {
                saw_newline_before_name |= self.token_text(token).contains('\n');
                index += 1;
                continue;
            }

            if at_top_level
                && saw_newline_after_name
                && token_kind_can_start_statement_after_local(kind)
            {
                return !saw_newline_before_name;
            }

            if at_top_level
                && saw_name_after_type
                && !saw_equal
                && kind == TokenKind::Operator(Operator::Less)
            {
                return false;
            }

            if at_top_level && matches!(kind, TokenKind::Semicolon | TokenKind::RightParen) {
                return saw_name_after_type;
            }
            if at_top_level && matches!(kind, TokenKind::RightBrace | TokenKind::Eof) {
                return false;
            }
            if at_top_level && !saw_equal && matches!(kind, TokenKind::Dot | TokenKind::Question) {
                return false;
            }
            if at_top_level && !saw_equal && kind == TokenKind::LeftParen {
                return false;
            }
            if at_top_level && is_assignment_operator(kind) && !saw_name_after_type {
                return false;
            }
            if at_top_level
                && is_assignment_operator(kind)
                && kind != TokenKind::Operator(Operator::Equal)
            {
                return false;
            }
            if at_top_level && kind == TokenKind::Operator(Operator::Equal) {
                saw_equal = true;
            }
            if at_top_level && !saw_equal && matches!(kind, TokenKind::Colon) {
                return false;
            }

            if at_top_level && index > start && kind == TokenKind::Identifier {
                if matches!(
                    self.next_non_trivia_kind_after(index + 1),
                    Some(TokenKind::Operator(
                        Operator::PlusPlus | Operator::MinusMinus
                    ))
                ) {
                    return false;
                }
                saw_name_after_type = true;
                saw_newline_after_name = false;
            }

            match kind {
                TokenKind::LeftParen => paren_depth += 1,
                TokenKind::RightParen => paren_depth = paren_depth.saturating_sub(1),
                TokenKind::LeftBracket => bracket_depth += 1,
                TokenKind::RightBracket => bracket_depth = bracket_depth.saturating_sub(1),
                TokenKind::LeftBrace => brace_depth += 1,
                TokenKind::RightBrace => brace_depth = brace_depth.saturating_sub(1),
                TokenKind::Operator(Operator::Less) if !saw_equal => angle_depth += 1,
                TokenKind::Operator(Operator::Greater) if !saw_equal => {
                    angle_depth = angle_depth.saturating_sub(1)
                }
                TokenKind::Operator(Operator::GreaterGreater) if !saw_equal => {
                    angle_depth = angle_depth.saturating_sub(2)
                }
                _ => {}
            }

            index += 1;
        }

        false
    }

    fn looks_like_local_decl_statement_in_for_header(&self) -> bool {
        if !is_declaration_start(self.current().kind) || self.at_keyword(Keyword::New) {
            return false;
        }

        let mut index = self.position;
        let mut saw_name_after_type = false;
        let mut saw_equal = false;
        let mut paren_depth = 0usize;
        let mut bracket_depth = 0usize;
        let mut brace_depth = 0usize;
        let mut angle_depth = 0usize;

        while index < self.tokens.len() {
            let kind = self.tokens[index].kind;
            let at_top_level =
                paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0;

            if at_top_level && matches!(kind, TokenKind::Semicolon | TokenKind::RightParen) {
                return saw_name_after_type;
            }
            if at_top_level && matches!(kind, TokenKind::RightBrace | TokenKind::Eof) {
                return false;
            }
            if at_top_level && !saw_equal && matches!(kind, TokenKind::Dot | TokenKind::Question) {
                return false;
            }
            if at_top_level && is_assignment_operator(kind) && !saw_name_after_type {
                return false;
            }
            if at_top_level
                && is_assignment_operator(kind)
                && kind != TokenKind::Operator(Operator::Equal)
            {
                return false;
            }
            if at_top_level && kind == TokenKind::Operator(Operator::Equal) {
                saw_equal = true;
            }
            if at_top_level && !saw_equal && matches!(kind, TokenKind::Colon) {
                return false;
            }

            if at_top_level && index > self.position && kind == TokenKind::Identifier {
                saw_name_after_type = true;
            }

            match kind {
                TokenKind::LeftParen => paren_depth += 1,
                TokenKind::RightParen => paren_depth = paren_depth.saturating_sub(1),
                TokenKind::LeftBracket => bracket_depth += 1,
                TokenKind::RightBracket => bracket_depth = bracket_depth.saturating_sub(1),
                TokenKind::LeftBrace => brace_depth += 1,
                TokenKind::RightBrace => brace_depth = brace_depth.saturating_sub(1),
                TokenKind::Operator(Operator::Less) if !saw_equal => angle_depth += 1,
                TokenKind::Operator(Operator::Greater) if !saw_equal => {
                    angle_depth = angle_depth.saturating_sub(1)
                }
                TokenKind::Operator(Operator::GreaterGreater) if !saw_equal => {
                    angle_depth = angle_depth.saturating_sub(2)
                }
                _ => {}
            }

            index += 1;
        }

        false
    }
    fn current_binary_binding_power(&self) -> Option<(u8, u8, SyntaxKind)> {
        match self.peek_non_trivia_kind()? {
            TokenKind::Operator(Operator::Equal)
            | TokenKind::Operator(Operator::PlusEqual)
            | TokenKind::Operator(Operator::MinusEqual)
            | TokenKind::Operator(Operator::StarEqual)
            | TokenKind::Operator(Operator::SlashEqual)
            | TokenKind::Operator(Operator::PercentEqual)
            | TokenKind::Operator(Operator::AmpersandEqual)
            | TokenKind::Operator(Operator::PipeEqual)
            | TokenKind::Operator(Operator::CaretEqual)
            | TokenKind::Operator(Operator::LessLessEqual)
            | TokenKind::Operator(Operator::GreaterGreaterEqual) => {
                Some((2, 1, SyntaxKind::AssignmentExpression))
            }
            TokenKind::Operator(Operator::PipePipe) => Some((3, 4, SyntaxKind::BinaryExpression)),
            TokenKind::Operator(Operator::AmpersandAmpersand) => {
                Some((5, 6, SyntaxKind::BinaryExpression))
            }
            TokenKind::Operator(Operator::Pipe) => Some((7, 8, SyntaxKind::BinaryExpression)),
            TokenKind::Operator(Operator::Caret) => Some((9, 10, SyntaxKind::BinaryExpression)),
            TokenKind::Operator(Operator::Ampersand) => {
                Some((11, 12, SyntaxKind::BinaryExpression))
            }
            TokenKind::Operator(Operator::EqualEqual)
            | TokenKind::Operator(Operator::BangEqual) => {
                Some((13, 14, SyntaxKind::BinaryExpression))
            }
            TokenKind::Operator(Operator::Less)
            | TokenKind::Operator(Operator::LessEqual)
            | TokenKind::Operator(Operator::Greater)
            | TokenKind::Operator(Operator::GreaterEqual) => {
                Some((15, 16, SyntaxKind::BinaryExpression))
            }
            TokenKind::Operator(Operator::LessLess)
            | TokenKind::Operator(Operator::GreaterGreater) => {
                Some((17, 18, SyntaxKind::BinaryExpression))
            }
            TokenKind::Operator(Operator::Plus) | TokenKind::Operator(Operator::Minus) => {
                Some((19, 20, SyntaxKind::BinaryExpression))
            }
            TokenKind::Operator(Operator::Star)
            | TokenKind::Operator(Operator::Slash)
            | TokenKind::Operator(Operator::Percent) => {
                Some((21, 22, SyntaxKind::BinaryExpression))
            }
            _ => None,
        }
    }

    fn at_prefix_operator(&self) -> bool {
        matches!(
            self.current().kind,
            TokenKind::Operator(Operator::Plus)
                | TokenKind::Operator(Operator::Minus)
                | TokenKind::Operator(Operator::Bang)
                | TokenKind::Operator(Operator::Tilde)
                | TokenKind::Operator(Operator::PlusPlus)
                | TokenKind::Operator(Operator::MinusMinus)
        )
    }

    fn looks_like_generic_argument_list(&self) -> bool {
        let mut index = self.position;
        let mut depth = 0usize;

        while index < self.tokens.len() {
            match self.tokens[index].kind {
                TokenKind::Operator(Operator::Less) => depth += 1,
                TokenKind::Operator(Operator::Greater) => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return self.generic_argument_list_can_continue(index + 1);
                    }
                }
                TokenKind::Operator(Operator::GreaterGreater) => {
                    depth = depth.saturating_sub(2);
                    if depth == 0 {
                        return self.generic_argument_list_can_continue(index + 1);
                    }
                }
                TokenKind::Semicolon
                | TokenKind::LeftBrace
                | TokenKind::RightBrace
                | TokenKind::Eof => return false,
                _ => {}
            }
            index += 1;
        }

        false
    }

    fn generic_argument_list_can_continue(&self, mut index: usize) -> bool {
        while index < self.tokens.len() && self.tokens[index].kind.is_trivia() {
            index += 1;
        }
        matches!(
            self.tokens.get(index).map(|token| token.kind),
            Some(TokenKind::Dot | TokenKind::LeftParen | TokenKind::LeftBracket)
        )
    }

    fn next_non_trivia_is(&self, kind: TokenKind) -> bool {
        self.peek_non_trivia_kind() == Some(kind)
    }

    fn next_non_trivia_is_any(&self, kinds: &[TokenKind]) -> bool {
        self.peek_non_trivia_kind()
            .is_some_and(|kind| kinds.contains(&kind))
    }

    fn next_non_trivia_is_operator(&self, operator: Operator) -> bool {
        self.peek_non_trivia_kind() == Some(TokenKind::Operator(operator))
    }

    fn peek_non_trivia_kind(&self) -> Option<TokenKind> {
        let mut index = self.position;
        while index < self.tokens.len() && self.tokens[index].kind.is_trivia() {
            index += 1;
        }
        self.tokens.get(index).map(|token| token.kind)
    }

    fn next_non_trivia_kind_after(&self, mut index: usize) -> Option<TokenKind> {
        while index < self.tokens.len() && self.tokens[index].kind.is_trivia() {
            index += 1;
        }
        self.tokens.get(index).map(|token| token.kind)
    }

    fn next_significant_kind(&self, significant_offset: usize) -> Option<TokenKind> {
        let mut seen = 0usize;
        for token in self.tokens.iter().skip(self.position) {
            if token.kind.is_trivia() {
                continue;
            }
            if seen == significant_offset {
                return Some(token.kind);
            }
            seen += 1;
        }
        None
    }

    fn next_token_can_start_expression(&self) -> bool {
        matches!(
            self.peek_non_trivia_kind(),
            Some(
                TokenKind::Identifier
                    | TokenKind::Keyword(_)
                    | TokenKind::Number
                    | TokenKind::String
                    | TokenKind::LeftParen
                    | TokenKind::LeftBrace
                    | TokenKind::Operator(Operator::Plus)
                    | TokenKind::Operator(Operator::Minus)
                    | TokenKind::Operator(Operator::Bang)
                    | TokenKind::Operator(Operator::Tilde)
                    | TokenKind::Operator(Operator::PlusPlus)
                    | TokenKind::Operator(Operator::MinusMinus)
            )
        )
    }

    fn at_modifier(&self) -> bool {
        matches!(
            self.current().kind,
            TokenKind::Keyword(Keyword::Modded)
                | TokenKind::Keyword(Keyword::Sealed)
                | TokenKind::Keyword(Keyword::Proto)
                | TokenKind::Keyword(Keyword::External)
                | TokenKind::Keyword(Keyword::Native)
                | TokenKind::Keyword(Keyword::Volatile)
                | TokenKind::Keyword(Keyword::Private)
                | TokenKind::Keyword(Keyword::Protected)
                | TokenKind::Keyword(Keyword::Static)
                | TokenKind::Keyword(Keyword::Override)
                | TokenKind::Keyword(Keyword::Const)
                | TokenKind::Keyword(Keyword::Owned)
                | TokenKind::Keyword(Keyword::Event)
        )
    }

    fn is_name_token(&self) -> bool {
        matches!(
            self.current().kind,
            TokenKind::Identifier | TokenKind::Keyword(_)
        )
    }

    fn at(&self, kind: TokenKind) -> bool {
        self.current().kind == kind
    }

    fn at_keyword(&self, keyword: Keyword) -> bool {
        self.current().kind == TokenKind::Keyword(keyword)
    }

    fn at_operator(&self, operator: Operator) -> bool {
        self.current().kind == TokenKind::Operator(operator)
    }

    fn current(&self) -> Token {
        self.tokens[self.position]
    }

    fn assert_progress(&self, start: usize, context: &'static str) {
        if self.position <= start {
            eprintln!(
                "fatal parser invariant violation: made no progress while parsing {context} at token {:?} span {:?}",
                self.current().kind,
                self.current().span
            );
            std::process::abort();
        }
    }

    fn bump_token(&mut self) -> SyntaxElement {
        let token = self.current();
        self.position += 1;
        SyntaxElement::Token(token)
    }

    fn with_recursion_budget(
        &mut self,
        stop: &[TokenKind],
        parse: impl FnOnce(&mut Self) -> SyntaxElement,
    ) -> SyntaxElement {
        if self.recursion_depth == MAX_RECURSION_DEPTH {
            self.error_here("Parser recursion limit exceeded");
            self.recursion_limit_recovered = true;

            let mut children = Vec::new();
            let mut delimiters = Vec::new();
            while !self.at(TokenKind::Eof) {
                let kind = self.current().kind;
                if kind == TokenKind::Semicolon
                    || (delimiters.is_empty()
                        && (stop.contains(&kind) || kind == TokenKind::RightBrace))
                {
                    break;
                }

                match kind {
                    TokenKind::LeftParen => delimiters.push(TokenKind::RightParen),
                    TokenKind::LeftBracket => delimiters.push(TokenKind::RightBracket),
                    TokenKind::LeftBrace => delimiters.push(TokenKind::RightBrace),
                    _ if delimiters.last().is_some_and(|expected| *expected == kind) => {
                        delimiters.pop();
                    }
                    _ => {}
                }
                children.push(self.bump_token());
            }
            return node(SyntaxKind::Error, children);
        }

        self.recursion_depth += 1;
        let result = parse(self);
        self.recursion_depth -= 1;
        if self.recursion_depth == 0 {
            self.recursion_limit_recovered = false;
        }
        result
    }

    fn expect(&mut self, kind: TokenKind, children: &mut Vec<SyntaxElement>, message: &str) {
        if self.at(kind) {
            children.push(self.bump_token());
        } else if !self.recursion_limit_recovered {
            self.error_here(message);
        }
    }

    fn error_here(&mut self, message: &str) {
        if self.recursion_limit_recovered {
            return;
        }
        self.error_at_span(message, self.current().span);
    }

    fn error_at_span(&mut self, message: &str, span: TextSpan) {
        self.diagnostics.push(ParseDiagnostic {
            message: message.to_string(),
            span,
        });
    }

    fn token_text(&self, token: Token) -> &str {
        &self.source[token.span.start..token.span.end]
    }

    fn token_ends_physical_line(&self, token: Token) -> bool {
        self.token_text(token)
            .chars()
            .any(|character| matches!(character, '\r' | '\n'))
    }
}

fn node(kind: SyntaxKind, children: Vec<SyntaxElement>) -> SyntaxElement {
    SyntaxElement::Node(Box::new(SyntaxNode::new(kind, children)))
}

/// Turns the token-preserving declaration tail into the one CST shape shared by
/// fields, locals, and declaration-form `for` initializers.  The declaration
/// recognizers above have already established that this is a declaration; this
/// helper only assigns its syntactic boundaries and deliberately leaves an
/// unrecognizable tail untouched for recovery.
fn structure_declaration(mut children: Vec<SyntaxElement>) -> Vec<SyntaxElement> {
    let Some(first_name) = first_declarator_element(&children) else {
        if is_type_reference_without_declarator(&children) {
            return vec![node(SyntaxKind::TypeRef, children)];
        }
        return children;
    };

    let mut result = children.drain(..first_name).collect::<Vec<_>>();
    if !result.is_empty() {
        let type_children = std::mem::take(&mut result);
        result.push(node(SyntaxKind::TypeRef, type_children));
    }

    let mut list = Vec::new();
    let mut declarator = Vec::new();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut terminator = Vec::new();

    for element in children {
        let kind = match &element {
            SyntaxElement::Token(token) => Some(token.kind),
            SyntaxElement::Node(_) => None,
        };
        let at_top_level =
            paren_depth == 0 && bracket_depth == 0 && angle_depth == 0 && brace_depth == 0;
        if at_top_level && kind == Some(TokenKind::Comma) {
            list.push(node(
                SyntaxKind::Declarator,
                std::mem::take(&mut declarator),
            ));
            list.push(element);
            continue;
        }
        if at_top_level && kind == Some(TokenKind::Semicolon) {
            terminator.push(element);
            continue;
        }
        if let Some(kind) = kind {
            update_declarator_depths(
                kind,
                &mut paren_depth,
                &mut bracket_depth,
                &mut angle_depth,
                &mut brace_depth,
            );
        }
        declarator.push(element);
    }
    if !declarator.is_empty() {
        list.push(node(SyntaxKind::Declarator, declarator));
    }
    if list.iter().any(|element| matches!(element, SyntaxElement::Node(node) if node.kind == SyntaxKind::Declarator)) {
        result.push(node(SyntaxKind::DeclaratorList, list));
        result.extend(terminator);
        result
    } else {
        // This cannot happen for a recognized first name, but retaining the
        // original tail is safer than manufacturing a declaration fact.
        result.into_iter().flat_map(|element| match element {
            SyntaxElement::Node(node) if node.kind == SyntaxKind::TypeRef => node.children,
            other => vec![other],
        }).collect()
    }
}

fn structure_foreach_variable(mut children: Vec<SyntaxElement>) -> Vec<SyntaxElement> {
    let Some(name_index) = first_declarator_element(&children) else {
        return children;
    };
    let mut result = children.drain(..name_index).collect::<Vec<_>>();
    if !result.is_empty() {
        result = vec![node(SyntaxKind::TypeRef, result)];
    }
    result.push(node(SyntaxKind::Declarator, children));
    result
}

fn first_declarator_element(children: &[SyntaxElement]) -> Option<usize> {
    let mut candidate = None;
    let mut saw_type_atom = false;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut brace_depth = 0usize;
    for (index, element) in children.iter().enumerate() {
        let SyntaxElement::Token(token) = element else {
            continue;
        };
        let at_top_level =
            paren_depth == 0 && bracket_depth == 0 && angle_depth == 0 && brace_depth == 0;
        if at_top_level
            && matches!(
                token.kind,
                TokenKind::Comma | TokenKind::Semicolon | TokenKind::Operator(Operator::Equal)
            )
        {
            break;
        }
        if at_top_level {
            match token.kind {
                TokenKind::Identifier => {
                    if saw_type_atom {
                        candidate = Some(index);
                    } else {
                        saw_type_atom = true;
                    }
                }
                TokenKind::Keyword(keyword) if is_type_head_keyword(keyword) => {
                    saw_type_atom = true;
                }
                _ => {}
            }
        }
        update_declarator_depths(
            token.kind,
            &mut paren_depth,
            &mut bracket_depth,
            &mut angle_depth,
            &mut brace_depth,
        );
    }
    candidate
}

fn is_type_head_keyword(keyword: Keyword) -> bool {
    matches!(
        keyword,
        Keyword::Void
            | Keyword::Int
            | Keyword::Float
            | Keyword::Bool
            | Keyword::String
            | Keyword::Vector
            | Keyword::Typename
            | Keyword::Auto
            | Keyword::Func
    )
}

fn is_type_reference_without_declarator(children: &[SyntaxElement]) -> bool {
    // A declaration parser reaches this recovery path only after recognizing
    // declaration-shaped input. Preserve its leading type instead of inventing
    // a variable when the user has not supplied a declarator yet.
    let mut tokens = children.iter().filter_map(|element| match element {
        SyntaxElement::Token(token) if !token.kind.is_trivia() => Some(*token),
        _ => None,
    });
    let Some(head) = tokens.next() else {
        return false;
    };
    matches!(head.kind, TokenKind::Identifier)
}

fn update_declarator_depths(
    kind: TokenKind,
    paren: &mut usize,
    bracket: &mut usize,
    angle: &mut usize,
    brace: &mut usize,
) {
    match kind {
        TokenKind::LeftParen => *paren += 1,
        TokenKind::RightParen => *paren = paren.saturating_sub(1),
        TokenKind::LeftBracket => *bracket += 1,
        TokenKind::RightBracket => *bracket = bracket.saturating_sub(1),
        TokenKind::LeftBrace => *brace += 1,
        TokenKind::RightBrace => *brace = brace.saturating_sub(1),
        TokenKind::Operator(Operator::Less) => *angle += 1,
        TokenKind::Operator(Operator::Greater) => *angle = angle.saturating_sub(1),
        TokenKind::Operator(Operator::GreaterGreater) => *angle = angle.saturating_sub(2),
        _ => {}
    }
}

fn single_or_wrapped_expression(mut children: Vec<SyntaxElement>) -> SyntaxElement {
    if children.len() == 1 {
        children.remove(0)
    } else {
        node(SyntaxKind::Expression, children)
    }
}

fn token_kind_can_start_statement_after_local(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Identifier
            | TokenKind::Keyword(Keyword::If)
            | TokenKind::Keyword(Keyword::For)
            | TokenKind::Keyword(Keyword::Foreach)
            | TokenKind::Keyword(Keyword::While)
            | TokenKind::Keyword(Keyword::Do)
            | TokenKind::Keyword(Keyword::Switch)
            | TokenKind::Keyword(Keyword::Return)
            | TokenKind::Keyword(Keyword::Break)
            | TokenKind::Keyword(Keyword::Continue)
            | TokenKind::Keyword(Keyword::Delete)
            | TokenKind::Keyword(Keyword::Thread)
    )
}

fn local_decl_expression_stops(stop: &[TokenKind]) -> Vec<TokenKind> {
    let mut stops = vec![TokenKind::Comma];
    stops.extend_from_slice(stop);
    stops
}

fn children_span(children: &[SyntaxElement]) -> TextSpan {
    let Some(first) = children.first() else {
        return TextSpan::new(0, 0);
    };
    let Some(last) = children.last() else {
        return first.span();
    };

    TextSpan::new(first.span().start, last.span().end)
}

fn is_declaration_start(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Identifier
            | TokenKind::Keyword(Keyword::Void)
            | TokenKind::Keyword(Keyword::Int)
            | TokenKind::Keyword(Keyword::Float)
            | TokenKind::Keyword(Keyword::Bool)
            | TokenKind::Keyword(Keyword::String)
            | TokenKind::Keyword(Keyword::Vector)
            | TokenKind::Keyword(Keyword::Typename)
            | TokenKind::Keyword(Keyword::Const)
            | TokenKind::Keyword(Keyword::Ref)
            | TokenKind::Keyword(Keyword::Notnull)
            | TokenKind::Keyword(Keyword::Auto)
            | TokenKind::Keyword(Keyword::Func)
    )
}

fn is_declaration_or_modifier_start(kind: TokenKind) -> bool {
    is_declaration_start(kind)
        || matches!(
            kind,
            TokenKind::Keyword(Keyword::Private)
                | TokenKind::Keyword(Keyword::Protected)
                | TokenKind::Keyword(Keyword::Static)
                | TokenKind::Keyword(Keyword::Override)
                | TokenKind::Keyword(Keyword::Proto)
                | TokenKind::Keyword(Keyword::Native)
                | TokenKind::Keyword(Keyword::External)
                | TokenKind::Keyword(Keyword::Sealed)
                | TokenKind::Keyword(Keyword::Modded)
                | TokenKind::Keyword(Keyword::Vanilla)
        )
}

fn is_declaration_sync_start(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Keyword(Keyword::Class)
            | TokenKind::Keyword(Keyword::Enum)
            | TokenKind::Keyword(Keyword::Typedef)
            | TokenKind::Keyword(Keyword::Modded)
            | TokenKind::Keyword(Keyword::Vanilla)
    )
}

fn is_declaration_recovery_sync_keyword(keyword: Keyword) -> bool {
    matches!(
        keyword,
        Keyword::Class
            | Keyword::Enum
            | Keyword::Typedef
            | Keyword::Modded
            | Keyword::Vanilla
            | Keyword::Private
            | Keyword::Protected
            | Keyword::Static
            | Keyword::Override
            | Keyword::Proto
            | Keyword::Native
            | Keyword::External
            | Keyword::Sealed
            | Keyword::Event
            | Keyword::Const
            | Keyword::Ref
            | Keyword::Notnull
            | Keyword::Auto
            | Keyword::Func
            | Keyword::Void
            | Keyword::Int
            | Keyword::Float
            | Keyword::Bool
            | Keyword::String
            | Keyword::Vector
            | Keyword::Typename
    )
}

fn is_assignment_operator(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Operator(Operator::Equal)
            | TokenKind::Operator(Operator::PlusEqual)
            | TokenKind::Operator(Operator::MinusEqual)
            | TokenKind::Operator(Operator::StarEqual)
            | TokenKind::Operator(Operator::SlashEqual)
            | TokenKind::Operator(Operator::PercentEqual)
            | TokenKind::Operator(Operator::AmpersandEqual)
            | TokenKind::Operator(Operator::PipeEqual)
            | TokenKind::Operator(Operator::CaretEqual)
            | TokenKind::Operator(Operator::LessLessEqual)
            | TokenKind::Operator(Operator::GreaterGreaterEqual)
    )
}

fn partial_declaration_has_declarator(children: &[SyntaxElement]) -> bool {
    let tokens = significant_declarator_tokens_in_elements(children);
    let name_like_count = tokens
        .iter()
        .filter(|token| matches!(token.kind, TokenKind::Identifier | TokenKind::Keyword(_)))
        .count();
    let last_kind = tokens.last().map(|token| token.kind);

    name_like_count >= 2
        && matches!(
            last_kind,
            Some(TokenKind::Identifier | TokenKind::RightBracket)
        )
}

fn significant_declarator_tokens_in_elements(children: &[SyntaxElement]) -> Vec<Token> {
    let mut tokens = Vec::new();
    for child in children {
        collect_significant_declarator_tokens(child, &mut tokens);
    }
    tokens
}

fn collect_significant_declarator_tokens(element: &SyntaxElement, tokens: &mut Vec<Token>) {
    match element {
        SyntaxElement::Token(token)
            if !token.kind.is_trivia() && token.kind != TokenKind::Semicolon =>
        {
            tokens.push(*token);
        }
        SyntaxElement::Node(node)
            if !matches!(
                node.kind,
                SyntaxKind::AttributeList | SyntaxKind::ModifierList
            ) =>
        {
            for child in &node.children {
                collect_significant_declarator_tokens(child, tokens);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{AstSourceFile, ClassMember, Declaration};
    use crate::index::SymbolIndex;
    use crate::lexer::{lex, TokenKind};
    use crate::model::SourceFileMetadata;
    use crate::semantic_file::SemanticFile;
    use crate::syntax::SyntaxKind;

    #[test]
    fn prelexed_parse_preserves_lossless_syntax_and_error_recovery() {
        for source in [
            "",
            "// comment\r\nclass Example { string label = \"café\"; }",
            "#ifdef WORKBENCH\nclass Example { ref array<int> values; }\n#endif",
            "class Broken { void Run( { string label = \"unterminated",
        ] {
            let tokens = lex(source);
            let before = crate::lexer::test_lex_call_count();
            let actual = parse_lexed_source(source, &tokens);
            assert_eq!(crate::lexer::test_lex_call_count(), before);
            assert_eq!(actual, parse_source(source), "{source}");
        }
    }

    fn count_kind(node: &SyntaxNode, kind: SyntaxKind) -> usize {
        let own = usize::from(node.kind == kind);
        own + node
            .children
            .iter()
            .map(|child| match child {
                SyntaxElement::Node(node) => count_kind(node, kind),
                SyntaxElement::Token(_) => 0,
            })
            .sum::<usize>()
    }

    fn first_node(node: &SyntaxNode, kind: SyntaxKind) -> Option<&SyntaxNode> {
        if node.kind == kind {
            return Some(node);
        }

        node.children.iter().find_map(|child| match child {
            SyntaxElement::Node(node) => first_node(node, kind),
            SyntaxElement::Token(_) => None,
        })
    }

    #[test]
    fn structures_field_local_and_for_declarators_in_the_cst() {
        let source = r#"class Example {
            protected ref array<int> values, other = { 1, 2 };
            void Run() {
                vector point[2] = { 1, 2, 3 };
                for (int index = 0, limit = 4; index < limit; index++) {}
            }
        }"#;
        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::TypeRef), 3);
        assert_eq!(count_kind(&parse.root, SyntaxKind::DeclaratorList), 3);
        assert_eq!(count_kind(&parse.root, SyntaxKind::Declarator), 5);
        assert_eq!(
            count_kind(&parse.root, SyntaxKind::InitializerExpression),
            2
        );
    }

    fn direct_child_node_count(node: &SyntaxNode, kind: SyntaxKind) -> usize {
        node.children
            .iter()
            .filter(|child| matches!(child, SyntaxElement::Node(node) if node.kind == kind))
            .count()
    }

    fn non_eof_token_count(source: &str) -> usize {
        lex(source)
            .into_iter()
            .filter(|token| token.kind != TokenKind::Eof)
            .count()
    }

    #[test]
    fn parses_declaration_shapes() {
        let source = r#"[BaseContainerProps(configRoot: true)]
class SCR_Example : Managed
{
	[Attribute()]
	protected ref array<ref SCR_Item> m_aItems = {};
	proto native bool Find(TKey key, out TValue value);
}

typedef map<ref Managed, ref Managed> TManagedRefManagedRefMap;
"#;

        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ClassDecl), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::AttributeList), 2);
        assert_eq!(count_kind(&parse.root, SyntaxKind::Attribute), 2);
        assert_eq!(count_kind(&parse.root, SyntaxKind::AttributeArgs), 2);
        assert_eq!(count_kind(&parse.root, SyntaxKind::MethodDecl), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::FieldDecl), 1);
        assert_eq!(
            count_kind(&parse.root, SyntaxKind::InitializerExpression),
            1
        );
        assert_eq!(count_kind(&parse.root, SyntaxKind::TypedefDecl), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ParameterList), 1);
    }

    #[test]
    fn keeps_generic_type_commas_inside_parameters() {
        let source = r#"class Example
{
	proto int Copy(map<TKey,TValue> from);
	static override bool GetEntitySourceBudgetCost(IEntityComponentSource editableEntitySource, out notnull array<ref SCR_EntityBudgetValue> budgetValues);
	void WithDefault(int value = Math.Clamp(1, 2, 3), string name = "ok");
	void WithBraceDefault(vector targetPosition[4] = { "1 0 0", "0 1 0", "0 0 1", "0 0 0" }, bool disableInput = false, SCR_LoiterCustomAnimData customAnimData = SCR_LoiterCustomAnimData.Default);
}
"#;

        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::MethodDecl), 4);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ParameterList), 4);
        assert_eq!(count_kind(&parse.root, SyntaxKind::Parameter), 8);
        assert!(count_kind(&parse.root, SyntaxKind::CallExpression) >= 1);
        assert!(count_kind(&parse.root, SyntaxKind::MemberAccessExpression) >= 2);
    }

    #[test]
    fn separates_field_initializer_expressions_from_blocks() {
        let source = r#"class Example
{
	protected ref array<int> m_aValues = {};
	static const ref array<string> NAMES = {"A", "B"};
	void Run()
	{
	}
}
"#;

        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::FieldDecl), 2);
        assert_eq!(
            count_kind(&parse.root, SyntaxKind::InitializerExpression),
            2
        );
        assert_eq!(count_kind(&parse.root, SyntaxKind::Block), 2);
    }

    #[test]
    fn parses_field_initializer_expressions() {
        let source = r#"class Example
{
	ref SCR_BTParam<bool> m_Value = new SCR_BTParam<bool>(SCR_AIActionTask.WAYPOINT_RELATED_PORT);
	int a = Math.Clamp(1, 2, 3), b = Other.Value;
}
"#;
        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::FieldDecl), 2);
        assert!(count_kind(&parse.root, SyntaxKind::NewExpression) >= 1);
        assert!(count_kind(&parse.root, SyntaxKind::CallExpression) >= 1);
        assert!(count_kind(&parse.root, SyntaxKind::MemberAccessExpression) >= 3);
    }

    #[test]
    fn keeps_nested_initializer_braces_inside_field_call_initializers() {
        let source = r#"class Example
{
	protected static ref TStringArray s_aVarsOut2 = SCR_AINodePortsHelpers.MergeTwoArrays(SCR_AIGetWaypointParameters.s_aVarsOut_Base, {PORT_ENTITY});
	protected ref array<int> m_aValues = {};
}
"#;

        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::FieldDecl), 2);
        assert_eq!(
            count_kind(&parse.root, SyntaxKind::InitializerExpression),
            2
        );
    }

    #[test]
    fn accepts_game_data_optional_semicolons_after_attributes_and_bodies() {
        let source = r#"class ScriptedLoadContainer: LoadContainer
{
	event protected bool StartObject() {return false;};
	event protected bool StartArray(out int count) {return false;};
}

[BaseContainerProps()]
class SCR_DefendWaypointPreset
{
	[Attribute("", UIWidgets.EditBox, "Preset name, only informative. Switch using index.")];
	protected string m_sName;

	[Attribute("true", UIWidgets.CheckBox, "Use turrets?")];
	protected bool m_bUseTurrets;
}
"#;

        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ClassDecl), 2);
        assert_eq!(count_kind(&parse.root, SyntaxKind::MethodDecl), 2);
        assert_eq!(count_kind(&parse.root, SyntaxKind::FieldDecl), 2);
        assert_eq!(count_kind(&parse.root, SyntaxKind::AttributeList), 3);
    }

    #[test]
    fn preserves_empty_semicolon_declarations() {
        let source = r#";
class Example
{
	protected ref array<WeaponSlotComponent> m_aWeaponSlots = new array<WeaponSlotComponent>(); ;
	proto external void RequestPlayerSave(int iPlayerId);;
}
"#;

        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ClassDecl), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::FieldDecl), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::MethodDecl), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::EmptyDecl), 3);
    }

    #[test]
    fn tolerates_field_before_class_close_without_semicolon() {
        let source = r#"class SerializerDefaultSpawnData: Managed
{
	vector Transform[4];
	ResourceName Prefab
}
"#;

        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ClassDecl), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::FieldDecl), 2);
        assert_eq!(count_kind(&parse.root, SyntaxKind::Block), 1);
    }

    #[test]
    fn unterminated_field_stops_before_following_callable_member() {
        let source = r#"class Example
{
	string m_Tag
	void Example(string tag)
	{
		m_Tag = tag;
	}

	protected vector m_Target[4]

	//-------------------------------------------------------------------------
	//! Calculates the position.
	protected void GetTarget(out vector target[4])
	{
		target = m_Target;
	}

	protected SCR_Component m_Component
	protected bool m_bEnabled;
}
"#;

        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ClassDecl), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::FieldDecl), 4);
        assert_eq!(count_kind(&parse.root, SyntaxKind::MethodDecl), 2);
    }

    #[test]
    fn preprocessor_invalid_branch_text_does_not_swallow_later_declarations() {
        let source = r#"#ifdef BREAK_COMPILATION
	THIS DEFINE BREAKS GAME SCRIPT MODULE COMPILATION
	DO NOT REMOVE IT
#endif

class ArmaReforgerScripted : ChimeraGame
{
}

ArmaReforgerScripted g_ARGame;
"#;

        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ClassDecl), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::FieldDecl), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::Error), 1);
    }

    #[test]
    fn preprocessor_directives_end_at_cr_and_crlf_line_endings() {
        for source in [
            "#define FLAG 1\rclass CrOnly {}",
            "#define FLAG 1\r\nclass CrLf {}",
        ] {
            let parse = parse_source(source);

            assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
            assert_eq!(
                count_kind(&parse.root, SyntaxKind::PreprocessorDirective),
                1
            );
            assert_eq!(count_kind(&parse.root, SyntaxKind::ClassDecl), 1);
        }
    }

    #[test]
    fn reports_unmatched_top_level_right_brace_without_dropping_it() {
        let parse = parse_source("}");

        assert_eq!(parse.diagnostics.len(), 1, "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::Error), 1);
        assert_eq!(parse.root.token_count(), non_eof_token_count("}") + 1);
    }

    #[test]
    fn recursion_limit_recovers_deep_editor_input_without_dropping_following_declarations() {
        let deep = MAX_RECURSION_DEPTH + 32;
        let expressions = [
            format!(
                "class Parent {{ int m_Value = {}1{}; }} class After {{}}",
                "(".repeat(deep),
                ")".repeat(deep)
            ),
            format!(
                "class Parent {{ int m_Value = {}1; }} class After {{}}",
                "(".repeat(deep)
            ),
            format!(
                "class Parent {{ int m_Value = {}1{}; }} class After {{}}",
                "!".repeat(deep),
                ""
            ),
            format!(
                "class Parent {{ int m_Value = {}1{}; }} class After {{}}",
                "{".repeat(deep),
                "}".repeat(deep)
            ),
        ];

        for source in expressions {
            let parse = parse_source(&source);

            assert_eq!(
                parse.diagnostics.len(),
                1,
                "{source}: {:?}",
                parse.diagnostics
            );
            assert_eq!(count_kind(&parse.root, SyntaxKind::Error), 1, "{source}");
            assert_eq!(
                count_kind(&parse.root, SyntaxKind::ClassDecl),
                2,
                "{source}"
            );
            assert_eq!(
                parse.root.token_count(),
                non_eof_token_count(&source) + 1,
                "{source}"
            );
        }

        let source = format!(
            "class Parent {{ void Run() {{ {} int m_Following; }} }} class After {{}}",
            format!("{}{}", "{".repeat(deep), "}".repeat(deep))
        );
        let parse = parse_source(&source);
        assert_eq!(parse.diagnostics.len(), 1, "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::Error), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ClassDecl), 2);
        assert_eq!(count_kind(&parse.root, SyntaxKind::LocalDeclStatement), 1);

        let semantic_file = SemanticFile::build(&source, &parse);
        let index =
            SymbolIndex::from_semantic_files([(&semantic_file, SourceFileMetadata::unknown())]);
        assert!(!index.files().is_empty());
    }

    #[test]
    fn invalid_top_level_text_does_not_swallow_following_class() {
        let source = r#"class BeforeInvalid
{
	void Run();
}

this is not a valid declaration

class AfterInvalid
{
	void Run();
}
"#;

        let parse = parse_source(source);

        assert_eq!(parse.diagnostics.len(), 1, "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ClassDecl), 2);
        assert_eq!(count_kind(&parse.root, SyntaxKind::Error), 1);
    }

    #[test]
    fn naked_block_in_class_body_recovers_without_looping() {
        let source = r#"class Example
{
	int m_Value;

	{
		int invalidBlockLocal;
	}

	void Run();
}
"#;

        let parse = parse_source(source);

        assert_eq!(parse.diagnostics.len(), 1, "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ClassDecl), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::FieldDecl), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::MethodDecl), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::Error), 1);
    }

    #[test]
    fn invalid_class_body_text_does_not_swallow_following_attribute_member() {
        let source = r#"class Example
{
	this

	[RplRpc(RplChannel.Reliable, RplRcver.Server)]
	protected void RpcDo()
	{
	}
}
"#;

        let parse = parse_source(source);

        assert_eq!(parse.diagnostics.len(), 1, "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ClassDecl), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::AttributeList), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::MethodDecl), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::Error), 1);
    }

    #[test]
    fn unterminated_field_does_not_swallow_following_attribute_member() {
        let source = r#"class Example
{
	int garbage

	[RplRpc(RplChannel.Reliable, RplRcver.Server)]
	protected void RpcDo()
	{
	}
}
"#;

        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ClassDecl), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::AttributeList), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::MethodDecl), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::FieldDecl), 1);
    }

    #[test]
    fn static_array_field_suffix_is_not_treated_as_attribute_recovery_boundary() {
        let source = r#"class Example
{
	int values[COUNT];

	[Attribute()]
	int next;
}
"#;

        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::AttributeList), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::FieldDecl), 2);
        assert_eq!(count_kind(&parse.root, SyntaxKind::Error), 0);
    }

    #[test]
    fn parses_statement_and_expression_shapes_in_callable_bodies() {
        let source = r#"class Example
{
	void Run(array<IEntity> items, map<string, Widget> widgets, string key)
	{
		foreach (int index, IEntity item : items)
		{
			items[index].GetOrigin();
		}

		for (int i = items.Count() - 1; i >= 0; --i)
			widgets[key].SetVisible(true);

		SCR_WorkbenchHelper.PrintFormatDialog("Warning", level: LogLevel.WARNING);
		vector pos = { 1, 2, 3 };
		IEntity entity = new GenericEntity();
		set<IEntity> entities = new set<IEntity>;
		WorldEditorAPI worldEditorAPI = ((WorldEditor)Workbench.GetModule(WorldEditor)).GetApi();
		thread RunLater(entity);
		delete entity;
	}
}
"#;

        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ForeachStatement), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ForStatement), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ForHeader), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ForInitializer), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ForCondition), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ForIncrement), 1);
        let for_initializer =
            first_node(&parse.root, SyntaxKind::ForInitializer).expect("for initializer");
        assert_eq!(
            direct_child_node_count(for_initializer, SyntaxKind::LocalDeclStatement),
            1
        );
        assert_eq!(count_kind(&parse.root, SyntaxKind::ForeachVariableList), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ForeachVariable), 2);
        let foreach_variable =
            first_node(&parse.root, SyntaxKind::ForeachVariable).expect("foreach variable");
        assert_eq!(
            direct_child_node_count(foreach_variable, SyntaxKind::TypeRef),
            1
        );
        assert_eq!(
            direct_child_node_count(foreach_variable, SyntaxKind::Declarator),
            1
        );
        assert_eq!(count_kind(&parse.root, SyntaxKind::ForeachIterable), 1);
        assert!(count_kind(&parse.root, SyntaxKind::CallExpression) >= 5);
        assert!(count_kind(&parse.root, SyntaxKind::MemberAccessExpression) >= 4);
        assert!(count_kind(&parse.root, SyntaxKind::IndexExpression) >= 2);
        assert_eq!(count_kind(&parse.root, SyntaxKind::NamedArgument), 1);
        assert_eq!(
            count_kind(&parse.root, SyntaxKind::InitializerExpression),
            1
        );
        assert_eq!(count_kind(&parse.root, SyntaxKind::NewExpression), 2);
        assert_eq!(count_kind(&parse.root, SyntaxKind::CastExpression), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ThreadStatement), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::DeleteStatement), 1);
    }

    #[test]
    fn unterminated_call_before_next_statement_stays_an_expression() {
        let source = r#"class Example
{
	void Run(int value)
	{
		GetGame() // statement still being typed

		if (value > 0)
			return;
	}
}
"#;
        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::CallExpression), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ExpressionStatement), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::LocalDeclStatement), 0);
        assert_eq!(count_kind(&parse.root, SyntaxKind::IfStatement), 1);
    }

    #[test]
    fn bare_identifier_before_following_statement_stays_an_expression() {
        let before_control_statement = r#"class Example
{
	void Run()
	{
		asdasdsadasd
		if (true)
			return;
	}
}
"#;
        let before_call_statement = r#"class Example
{
	void Run()
	{
		asdasdsadasd
		DoThing();
	}
}
"#;
        let before_postfix_statement = r#"class Example
{
	void Run()
	{
		int testnum = 5;
		asdasdsadasd // Still showing as class green

		testnum++;
	}
}
"#;

        for (source, following_kind, expression_count, local_count) in [
            (before_control_statement, SyntaxKind::IfStatement, 1, 0),
            (before_call_statement, SyntaxKind::CallExpression, 2, 0),
            (
                before_postfix_statement,
                SyntaxKind::PostfixExpression,
                2,
                1,
            ),
        ] {
            let parse = parse_source(source);

            assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
            assert_eq!(
                count_kind(&parse.root, SyntaxKind::ExpressionStatement),
                expression_count
            );
            assert_eq!(
                count_kind(&parse.root, SyntaxKind::LocalDeclStatement),
                local_count
            );
            assert_eq!(count_kind(&parse.root, following_kind), 1);
        }
    }

    #[test]
    fn consecutive_unfinished_identifier_lines_do_not_form_a_local_declaration() {
        let source = r#"class Example
{
    void Run()
    {
        int testnum = 5;
        array<string> extra = {};

        SCR_BaseGameMode

        PS_PlayersList

        map<int, string> testmap = new map<int, string>();
    }
}
"#;

        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::LocalDeclStatement), 3);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ExpressionStatement), 2);
        assert_eq!(count_kind(&parse.root, SyntaxKind::Declarator), 3);
        assert_eq!(count_kind(&parse.root, SyntaxKind::Error), 0);

        let ast = AstSourceFile::new(source, &parse);
        let Declaration::Class(class) = ast.declarations()[0] else {
            panic!("expected class");
        };
        let ClassMember::Method(method) = class.members()[0] else {
            panic!("expected method");
        };
        let local_names = method
            .local_variables()
            .iter()
            .map(|local| local.name().text())
            .collect::<Vec<_>>();
        assert_eq!(local_names, vec!["testnum", "extra", "testmap"]);
    }

    #[test]
    fn dangling_assignment_does_not_consume_the_next_local_declaration() {
        let source = r#"class Example
{
	void Run()
	{
		IEntity testEntity =

		map<int, string> testmap = new map<int, string>();
	}
}
"#;

        let parse = parse_source(source);

        assert_eq!(count_kind(&parse.root, SyntaxKind::LocalDeclStatement), 2);
        assert_eq!(count_kind(&parse.root, SyntaxKind::Declarator), 2);
        assert_eq!(parse.diagnostics.len(), 1, "{:?}", parse.diagnostics);

        let ast = AstSourceFile::new(source, &parse);
        let Declaration::Class(class) = ast.declarations()[0] else {
            panic!("expected class");
        };
        let ClassMember::Method(method) = class.members()[0] else {
            panic!("expected method");
        };
        let local_names = method
            .local_variables()
            .iter()
            .map(|local| local.name().text())
            .collect::<Vec<_>>();
        assert_eq!(local_names, vec!["testEntity", "testmap"]);
    }

    #[test]
    fn multiline_local_initializer_remains_part_of_its_declaration() {
        let source = r#"class Example
{
	void Run()
	{
		IEntity testEntity =
			FindEntity();
		map<int, string> testmap = new map<int, string>();
	}
}
"#;

        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::LocalDeclStatement), 2);
        assert_eq!(count_kind(&parse.root, SyntaxKind::Declarator), 2);
        assert_eq!(count_kind(&parse.root, SyntaxKind::CallExpression), 1);
    }

    #[test]
    fn multiline_local_declaration_keeps_its_declarator() {
        let user_defined = r#"class Example
{
	void Run()
	{
		SomeType
			value;
	}
}
"#;
        let primitive = r#"class Example
{
	void Run()
	{
		int
			value;
	}
}
"#;
        let generic = r#"class Example
{
	void Run()
	{
		array<int>
			values;
	}
}
"#;

        for source in [user_defined, primitive, generic] {
            let parse = parse_source(source);

            assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
            assert_eq!(count_kind(&parse.root, SyntaxKind::LocalDeclStatement), 1);
            assert_eq!(count_kind(&parse.root, SyntaxKind::Declarator), 1);
            assert_eq!(count_kind(&parse.root, SyntaxKind::ExpressionStatement), 0);
        }
    }

    #[test]
    fn keeps_expression_form_for_initializer_expression_shaped() {
        let source = r#"class Example
{
	void Run(int count)
	{
		for (i = 0; i < count; i++)
			DoSomething(i);
	}
}
"#;

        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        let for_initializer =
            first_node(&parse.root, SyntaxKind::ForInitializer).expect("for initializer");
        assert_eq!(
            direct_child_node_count(for_initializer, SyntaxKind::LocalDeclStatement),
            0
        );
    }

    #[test]
    fn compound_assignment_statements_are_not_local_declarations() {
        let source = r#"class Example
{
	void Run()
	{
		string absPath;
		addonsDir += absPath;
		flags |= Flag.Enabled;
	}
}
"#;

        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::LocalDeclStatement), 1);
        assert!(count_kind(&parse.root, SyntaxKind::AssignmentExpression) >= 2);
    }

    #[test]
    fn parses_attribute_arguments_as_expressions() {
        let source = r#"class Example
{
	[Attribute("", UIWidgets.ComboBox, desc: "Display", enums: ParamEnumArray.FromEnum(EExample))]
	int value;
}
"#;

        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::AttributeArgs), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::NamedArgument), 2);
        assert!(count_kind(&parse.root, SyntaxKind::MemberAccessExpression) >= 2);
        assert!(count_kind(&parse.root, SyntaxKind::CallExpression) >= 1);
    }

    #[test]
    fn parses_switch_single_line_if_and_ternary_shapes() {
        let source = r#"class Example
{
	int Run(int value)
	{
		switch (value)
		{
			case 1:
			case 2:
			{
				if (value > 0)
					return value ? 1 : 2;
				break;
			}
			default:
				return 0;
		}
	}
}
"#;

        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::SwitchStatement), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::SwitchSection), 2);
        assert_eq!(count_kind(&parse.root, SyntaxKind::CaseClause), 2);
        assert_eq!(count_kind(&parse.root, SyntaxKind::DefaultClause), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::IfStatement), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::TernaryExpression), 1);
        assert_eq!(count_kind(&parse.root, SyntaxKind::BreakStatement), 1);
    }

    #[test]
    fn parses_unbraced_if_body_as_one_following_statement() {
        let source = r#"class Example
{
	void Run(bool enabled)
	{
		if (enabled)
			DoFirst();
		DoSecond();
	}
}
"#;

        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        let if_node = first_node(&parse.root, SyntaxKind::IfStatement).expect("if statement");
        assert_eq!(
            direct_child_node_count(if_node, SyntaxKind::ExpressionStatement),
            1
        );
        assert_eq!(count_kind(&parse.root, SyntaxKind::ExpressionStatement), 2);
        assert_eq!(count_kind(&parse.root, SyntaxKind::CallExpression), 2);
    }

    #[test]
    fn parses_inline_if_and_else_if_without_braces() {
        let source = r#"class Example
{
	bool Run(bool a, bool b)
	{
		if (a) return true;
		else if (b)
			return false;
		else
			return true;
	}
}
"#;

        let parse = parse_source(source);

        assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
        assert_eq!(count_kind(&parse.root, SyntaxKind::IfStatement), 2);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ElseClause), 2);
        assert_eq!(count_kind(&parse.root, SyntaxKind::ReturnStatement), 3);
        let if_node = first_node(&parse.root, SyntaxKind::IfStatement).expect("if statement");
        assert_eq!(direct_child_node_count(if_node, SyntaxKind::ElseClause), 1);
    }

    #[test]
    fn preserves_all_tokens_in_committed_parser_fixtures() {
        let fixtures = [
            include_str!("../../tools/fixtures/parser/core_types_declarations.c"),
            include_str!("../../tools/fixtures/parser/attributes_rpc_workbench.c"),
            include_str!("../../tools/fixtures/parser/modded_game_mode_members.c"),
            include_str!("../../tools/fixtures/parser/preprocessor_directives.c"),
            include_str!("../../tools/fixtures/parser/game_building_network_component.c"),
            include_str!("../../tools/fixtures/parser/game_building_provider_excerpt.c"),
            include_str!("../../tools/fixtures/parser/game_editable_group_excerpt.c"),
            include_str!("../../tools/fixtures/parser/game_editor_preview_params.c"),
            include_str!("../../tools/fixtures/parser/workbench_basic_code_formatter_excerpt.c"),
            include_str!("../../tools/fixtures/parser/game_optional_semicolon_shapes.c"),
            include_str!("../../tools/fixtures/parser/game_field_initializer_call_shapes.c"),
        ];

        for fixture in fixtures {
            let parse = parse_source(fixture);
            assert!(parse.diagnostics.is_empty(), "{:?}", parse.diagnostics);
            assert_eq!(parse.root.token_count(), non_eof_token_count(fixture) + 1);
        }
    }

    #[test]
    fn recovers_with_diagnostics_for_malformed_source() {
        let parse = parse_source("class MissingBody\n{\nvoid Bad(int value\n");

        assert!(!parse.diagnostics.is_empty());
        assert_eq!(parse.root.kind, SyntaxKind::SourceFile);
    }
}
