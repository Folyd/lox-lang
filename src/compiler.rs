use core::panic;
use std::{array, mem, ops::Add};

use crate::{
    object::{Function, FunctionType},
    scanner::{Scanner, Token, TokenType},
    vm::InterpretResult,
    OpCode, Value,
};
use num_enum::{IntoPrimitive, TryFromPrimitive};

const MAX_LOCAL_SIZE: usize = u8::MAX as usize + 1;
const UNINITIALIZED_LOCAL_DEPTH: isize = -1;

type ParseFn<'a> = fn(&mut Parser<'a>, bool /*can assign*/);

struct Parser<'a> {
    compiler: Box<Compiler<'a>>,
    scanner: Scanner<'a>,
    current: Token<'a>,
    previous: Token<'a>,
    // chunk: Chunk,
    had_error: bool,
    panic_mode: bool,
}

struct ParseRule<'a> {
    prefix: Option<ParseFn<'a>>,
    infix: Option<ParseFn<'a>>,
    precedence: Precedence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, IntoPrimitive, TryFromPrimitive)]
#[repr(u8)]
enum Precedence {
    None,
    Assignment, // =
    Or,         // or
    And,        // and
    Equality,   // == !=
    Comparison, // < > <= >=
    Term,       // + -
    Factor,     // * /
    Unary,      // ! -
    Call,       // . ()
    Primary,
}

impl Add<u8> for Precedence {
    type Output = Self;

    fn add(self, rhs: u8) -> Self::Output {
        Self::try_from(self as u8 + rhs).unwrap()
    }
}

#[derive(Debug, Clone, Default)]
struct Local<'a> {
    name: Token<'a>,
    // -1 (UNINITIALIZED_LOCAL_DEPTH) means no assigned yet.
    depth: isize,
    // This field is true if the local is captured by any later
    // nested function declaration. Initially, all locals are not captured.
    is_captured: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct Upvalue {
    index: usize,
    // that flag controls whether the closure captures a local
    // variable or an upvalue from the surrounding function.
    is_local: bool,
}

pub struct Compiler<'a> {
    enclosing: Option<Box<Compiler<'a>>>,
    function: Function,
    fn_type: FunctionType,
    locals: [Local<'a>; MAX_LOCAL_SIZE],
    local_count: usize,
    scope_depth: isize,
    upvalues: [Upvalue; MAX_LOCAL_SIZE],
}

impl<'a> Parser<'a> {
    fn new(source: &'a str) -> Self {
        Parser {
            // Let the default top level <script> function name to empty.
            compiler: Compiler::new(FunctionType::Script, ""),
            scanner: Scanner::new(source),
            current: Token::default(),
            previous: Token::default(),
            had_error: false,
            panic_mode: false,
        }
    }

    fn make_constant(&mut self, value: Value) -> usize {
        let constant = self.compiler.function.add_constant(value);
        if constant > u8::MAX as usize {
            self.error("Too many constants in one chunk.");
            return 0;
        }
        constant
    }

    fn emit_byte<T: Into<u8>>(&mut self, byte: T) {
        self.compiler.function.write_byte(byte, self.previous.line);
    }

    fn emit_bytes<T1: Into<u8>, T2: Into<u8>>(&mut self, byte1: T1, byte2: T2) {
        self.compiler.function.write_byte(byte1, self.previous.line);
        self.compiler.function.write_byte(byte2, self.previous.line);
    }

    fn emit_return(&mut self) {
        self.emit_byte(OpCode::Nil);
        self.emit_byte(OpCode::Return);
    }

    fn emit_constant(&mut self, value: Value) {
        let constant = self.make_constant(value);
        self.emit_bytes(OpCode::Constant, constant as u8);
    }

    fn emit_jump(&mut self, instruction: OpCode) -> usize {
        self.emit_byte(instruction);
        self.emit_byte(0xff);
        self.emit_byte(0xff);
        self.compiler.function.code_size() - 2
    }

    fn patch_jump(&mut self, offset: usize) {
        let jump = self.compiler.function.code_size() - offset - 2;
        if jump > u16::MAX as usize {
            self.error("Too much code to jump over.");
        }

        // self.compiler.function[offset] = ((jump >> 8) & 0xff) as u8;
        // self.compiler.function[offset + 1] = (jump & 0xff) as u8;
        let [a, b] = (jump as u16).to_be_bytes();
        self.compiler.function[offset] = a;
        self.compiler.function[offset + 1] = b;
    }

    fn emit_loop(&mut self, loop_start: usize) {
        self.emit_byte(OpCode::Loop);

        let jump = self.compiler.function.code_size() - loop_start + 2;
        if jump > u16::MAX as usize {
            self.error("Loop body too large.");
        }

        let [a, b] = (jump as u16).to_be_bytes();
        self.emit_byte(a);
        self.emit_byte(b);
    }
}

impl<'a> Parser<'a> {
    fn declaration(&mut self) {
        if self._match(TokenType::Fun) {
            self.fun_declaration();
        } else if self._match(TokenType::Var) {
            self.var_decaration();
        } else {
            self.statement();
        }

        if self.panic_mode {
            self.synchronize();
        }
    }

    fn synchronize(&mut self) {
        self.panic_mode = false;

        while self.current.kind != TokenType::Eof {
            if self.previous.kind == TokenType::Semicolon {
                return;
            }

            match self.current.kind {
                TokenType::Class
                | TokenType::Fun
                | TokenType::Var
                | TokenType::For
                | TokenType::If
                | TokenType::While
                | TokenType::Print
                | TokenType::Return => return,
                _ => self.advance(),
            }
        }
    }

    fn var_decaration(&mut self) {
        let global = self.parse_variable("Expect variable name.");

        if self._match(TokenType::Equal) {
            self.expression();
        } else {
            // desugars a variable declaration like: var a;
            // into: var a = nil;
            self.emit_byte(OpCode::Nil);
        }

        self.consume(
            TokenType::Semicolon,
            "Expect ';' after variable declaration.",
        );
        self.define_variable(global);
    }

    fn parse_variable(&mut self, error_message: &str) -> usize {
        self.consume(TokenType::Identifier, error_message);
        self.declare_varible();
        if self.compiler.scope_depth > 0 {
            // Encounter a local variable
            return 0;
        }
        self.identifier_constant(self.previous.lexeme)
    }

    fn identifier_constant(&mut self, identifier: &str) -> usize {
        self.make_constant(Value::from(identifier))
    }

    fn add_local(&mut self, name: Token<'a>) {
        if self.compiler.local_count == MAX_LOCAL_SIZE {
            self.error("Too many local variables in function.");
            return;
        }

        self.compiler.locals[self.compiler.local_count] = Local {
            name,
            depth: UNINITIALIZED_LOCAL_DEPTH,
            is_captured: false,
        };
        self.compiler.local_count += 1;
    }

    fn declare_varible(&mut self) {
        if self.compiler.scope_depth == 0 {
            // Encounter a global variable
            return;
        }
        for i in (0..self.compiler.local_count).rev() {
            let local = &self.compiler.locals[i];
            if local.depth != UNINITIALIZED_LOCAL_DEPTH && local.depth < self.compiler.scope_depth {
                break;
            }
            if local.name.lexeme == self.previous.lexeme {
                self.error("Already a variable with this name in this scope.");
            }
        }
        self.add_local(self.previous.clone());
    }

    fn mark_initialized(&mut self) {
        if self.compiler.scope_depth == 0 {
            return;
        }
        self.compiler.locals[self.compiler.local_count - 1].depth = self.compiler.scope_depth;
    }

    fn define_variable(&mut self, global: usize) {
        if self.compiler.scope_depth > 0 {
            // Encounter a local variable, set the depth to the current scope depth
            self.mark_initialized();
            return;
        }
        self.emit_bytes(OpCode::DefineGlobal, global as u8);
    }

    fn statement(&mut self) {
        if self._match(TokenType::Print) {
            self.print_statement();
        } else if self._match(TokenType::LeftBrace) {
            self.begin_scope();
            self.block();
            self.end_scope();
        } else if self._match(TokenType::If) {
            self.if_statement();
        } else if self._match(TokenType::Return) {
            self.return_statement();
        } else if self._match(TokenType::While) {
            self.while_statement();
        } else if self._match(TokenType::For) {
            self.for_statement();
        } else {
            self.expression_statement();
        }
    }

    fn return_statement(&mut self) {
        if self.compiler.fn_type == FunctionType::Script {
            self.error("Can't return from top-level code.");
        }

        if self._match(TokenType::Semicolon) {
            self.emit_return();
        } else {
            self.expression();
            self.consume(TokenType::Semicolon, "Expect ';' after return value.");
            self.emit_byte(OpCode::Return);
        }
    }

    fn if_statement(&mut self) {
        self.consume(TokenType::LeftParen, "Expect '(' after 'if'.");
        self.expression();
        self.consume(TokenType::RightParen, "Expect ')' after condition.");

        let then_jump = self.emit_jump(OpCode::JumpIfFalse);
        self.emit_byte(OpCode::Pop);
        self.statement();
        let else_jump = self.emit_jump(OpCode::Jump);
        self.patch_jump(then_jump);
        self.emit_byte(OpCode::Pop);

        if self._match(TokenType::Else) {
            self.statement();
        }
        self.patch_jump(else_jump);
    }

    fn while_statement(&mut self) {
        let loop_start = self.compiler.function.code_size();
        self.consume(TokenType::LeftParen, "Expect '(' after 'while'.");
        self.expression();
        self.consume(TokenType::RightParen, "Expect ')' after condition.");

        let exit_jump = self.emit_jump(OpCode::JumpIfFalse);
        self.emit_byte(OpCode::Pop);
        self.statement();
        self.emit_loop(loop_start);

        self.patch_jump(exit_jump);
        self.emit_byte(OpCode::Pop);
    }

    fn for_statement(&mut self) {
        self.begin_scope();
        self.consume(TokenType::LeftParen, "Expect '(' after 'for'.");
        if self._match(TokenType::Semicolon) {
            // No initializer.
        } else if self._match(TokenType::Var) {
            self.var_decaration();
        } else {
            self.expression_statement();
        }

        let mut loop_start = self.compiler.function.code_size();
        let mut exit_jump = None;
        if !self._match(TokenType::Semicolon) {
            self.expression();
            self.consume(TokenType::Semicolon, "Expect ';' after loop condition.");

            // Jump out of the loop if the condition is false.
            exit_jump = Some(self.emit_jump(OpCode::JumpIfFalse));
            self.emit_byte(OpCode::Pop);
        }

        if !self._match(TokenType::RightParen) {
            let body_jump = self.emit_jump(OpCode::Jump);
            let increment_start = self.compiler.function.code_size();
            self.expression();
            self.emit_byte(OpCode::Pop);
            self.consume(TokenType::RightParen, "Expect ')' after for clauses.");

            self.emit_loop(loop_start);
            loop_start = increment_start;
            self.patch_jump(body_jump);
        }

        self.statement();
        self.emit_loop(loop_start);

        if let Some(exit_jump) = exit_jump {
            // We do this only when there is a condition clause.
            // If there isn’t, there’s no jump to patch and no condition value on the stack to pop.
            self.patch_jump(exit_jump);
            self.emit_byte(OpCode::Pop);
        }
        self.end_scope();
    }

    fn fun_declaration(&mut self) {
        let global = self.parse_variable("Expect function name.");
        self.mark_initialized();
        self.function(FunctionType::Function);
        self.define_variable(global);
    }

    fn function(&mut self, fn_type: FunctionType) {
        self.push_compiler(fn_type);
        self.begin_scope();

        self.consume(TokenType::LeftParen, "Expect '(' after function name.");
        if !self.check(TokenType::RightParen) {
            loop {
                self.compiler.function.arity += 1;
                // if self.compiler.function.arity > 255 {
                //     self.error_at_current("Can't have more than 255 parameters.");
                // }
                let constant = self.parse_variable("Expect parameter name.");
                self.define_variable(constant);

                if !self._match(TokenType::Comma) {
                    break;
                }
            }
        }
        self.consume(TokenType::RightParen, "Expect ')' after parameters.");
        self.consume(TokenType::LeftBrace, "Expect '{' before function body.");
        self.block();

        // Pop the compiler of this compiling function,
        // self.compiler already became set to enclosing compiler
        let compiler = self.pop_compiler();
        let upvalue_count = compiler.function.upvalue_count;
        let constant = self.make_constant(Value::from(compiler.function));
        self.emit_bytes(OpCode::Closure, constant as u8);

        for i in 0..upvalue_count {
            // For each upvalue the closure captures, there are two single-byte operands.
            // Each pair of operands specifies what that upvalue captures.
            // If the first byte is one, it captures a local variable in the enclosing function.
            // If zero, it captures one of the function’s upvalues.
            // The next byte is the local slot or upvalue index to capture.
            let upvalue = compiler.upvalues[i as usize];
            self.emit_bytes(upvalue.is_local as u8, upvalue.index as u8);
        }
    }

    fn block(&mut self) {
        while !self.check(TokenType::RightBrace) && !self.check(TokenType::Eof) {
            self.declaration();
        }

        self.consume(TokenType::RightBrace, "Expect '}' after block.");
    }

    fn begin_scope(&mut self) {
        self.compiler.scope_depth += 1;
    }

    fn end_scope(&mut self) {
        self.compiler.scope_depth -= 1;

        while self.compiler.local_count > 0
            && self.compiler.locals[self.compiler.local_count - 1].depth > self.compiler.scope_depth
        {
            if self.compiler.locals[self.compiler.local_count - 1].is_captured {
                // Whenever the compiler reaches the end of a block, it discards all local 
                // variables in that block and emits an OpCode::CloseUpvalue for each local variable
                self.emit_byte(OpCode::CloseUpvalue);
            } else {
                // TODO: implement OP_POPN instruction that takes an operand
                // for the number of slots to pop and pops them all at once.
                self.emit_byte(OpCode::Pop);
            }

            self.compiler.local_count -= 1;
        }
    }

    fn _match(&mut self, kind: TokenType) -> bool {
        if self.check(kind) {
            self.advance();
            return true;
        }
        false
    }

    fn check(&mut self, kind: TokenType) -> bool {
        self.current.kind == kind
    }

    fn print_statement(&mut self) {
        self.expression();
        self.consume(TokenType::Semicolon, "Expect ';' after value.");
        self.emit_byte(OpCode::Print);
    }

    fn expression_statement(&mut self) {
        self.expression();
        self.consume(TokenType::Semicolon, "Expect ';' after expression.");
        // an expression statement evaluates the expression and discards the result
        // since the result already exists in the stack, we can just pop it
        self.emit_byte(OpCode::Pop);
    }
}

impl<'a> Parser<'a> {
    fn compile(&mut self) -> Result<Function, InterpretResult> {
        self.advance();
        while !self._match(TokenType::Eof) {
            self.declaration();
        }
        self.emit_return();
        if self.had_error {
            return Err(InterpretResult::CompileError);
        }
        Ok(mem::take(&mut self.compiler.function))
    }

    // fn end_compile(&mut self) -> Option<Function> {
    //     self.emit_return();
    //     self.pop_compiler()
    // }

    fn push_compiler(&mut self, fn_type: FunctionType) {
        // grab the function name from the previous token
        let function_name = self.previous.lexeme;
        let compiler = Compiler::new(fn_type, function_name);
        let enclosing_compiler = mem::replace(&mut self.compiler, compiler);
        self.compiler.enclosing = Some(enclosing_compiler);
    }

    fn pop_compiler(&mut self) -> Box<Compiler<'a>> {
        self.emit_return();
        if let Some(enclosing_compiler) = self.compiler.enclosing.take() {
            mem::replace(&mut self.compiler, enclosing_compiler)
        } else {
            panic!("No enclosing compiler to pop");
        }
    }

    fn advance(&mut self) {
        self.previous = mem::take(&mut self.current);
        while let Some(token) = self.scanner.next() {
            self.current = token;
            if self.previous.kind == TokenType::Bang
                && !matches!(
                    self.current.kind,
                    TokenType::True
                        | TokenType::False
                        | TokenType::Nil
                        | TokenType::Bang
                        | TokenType::LeftParen
                )
            {
                self.error("! operator can only be used on booleans and nil.");
                break;
            }

            if self.current.kind != TokenType::Error {
                break;
            }

            self.error_at_current(self.current.lexeme);
        }
    }

    fn expression(&mut self) {
        self.parse_precedence(Precedence::Assignment);
    }

    fn parse_precedence(&mut self, precedence: Precedence) {
        self.advance();
        let rule = ParseRule::get_rule(self.previous.kind);

        let can_assign = precedence <= Precedence::Assignment;
        if let Some(prefix_fn) = rule.prefix {
            prefix_fn(self, can_assign);
        } else {
            self.error("Parse precedence: expect expression.");
            return;
        }

        while precedence <= ParseRule::get_rule(self.current.kind).precedence {
            self.advance();
            // Do not reuse the previous rule since it may have changed.
            let rule = ParseRule::get_rule(self.previous.kind);
            if let Some(infix_fn) = rule.infix {
                infix_fn(self, can_assign);
            }
        }

        if can_assign && self._match(TokenType::Equal) {
            self.error("Invalid assignment target.");
        }
    }

    fn consume(&mut self, kind: TokenType, message: &str) {
        if self.current.kind == kind {
            self.advance();
            return;
        }
        self.error_at_current(message);
    }

    fn literal(&mut self, _can_assign: bool) {
        match self.previous.kind {
            TokenType::False => self.emit_byte(OpCode::False),
            TokenType::True => self.emit_byte(OpCode::True),
            TokenType::Nil => self.emit_byte(OpCode::Nil),
            _ => (),
        }
    }

    fn string(&mut self, _can_assign: bool) {
        let string = self.previous.lexeme.trim_matches('"');
        self.emit_constant(string.into());
    }

    fn number(&mut self, _can_assign: bool) {
        self.emit_constant(self.previous.lexeme.parse::<f64>().unwrap().into());
    }

    fn grouping(&mut self, _can_assign: bool) {
        self.expression();
        self.consume(TokenType::RightParen, "Expect ')' after expression.");
    }

    fn unary(&mut self, _can_assign: bool) {
        let operator_kind = self.previous.kind;
        self.parse_precedence(Precedence::Unary);
        match operator_kind {
            TokenType::Minus => self.emit_byte(OpCode::Negate),
            TokenType::Bang => self.emit_byte(OpCode::Not),
            _ => (),
        }
    }

    fn binary(&mut self, _can_assign: bool) {
        let operator_kind = self.previous.kind;
        let rule = ParseRule::get_rule(operator_kind);
        self.parse_precedence(rule.precedence + 1);

        match operator_kind {
            TokenType::Plus => self.emit_byte(OpCode::Add),
            TokenType::Minus => self.emit_byte(OpCode::Subtract),
            TokenType::Star => self.emit_byte(OpCode::Multiply),
            TokenType::Slash => self.emit_byte(OpCode::Divide),
            TokenType::BangEqual => self.emit_bytes(OpCode::Equal, OpCode::Not),
            TokenType::EqualEqual => self.emit_byte(OpCode::Equal),
            TokenType::Greater => self.emit_byte(OpCode::Greater),
            TokenType::GreaterEqual => self.emit_bytes(OpCode::Less, OpCode::Not),
            TokenType::Less => self.emit_byte(OpCode::Less),
            TokenType::LessEqual => self.emit_bytes(OpCode::Greater, OpCode::Not),
            _ => (),
        }
    }

    fn call(&mut self, _can_assign: bool) {
        let arg_count = self.argument_list();
        self.emit_bytes(OpCode::Call, arg_count);
    }

    fn argument_list(&mut self) -> u8 {
        let mut arg_count = 0;
        if !self.check(TokenType::RightParen) {
            loop {
                self.expression();

                if arg_count == 255 {
                    self.error("Can't have more than 255 arguments.");
                    break;
                }
                arg_count += 1;

                if !self._match(TokenType::Comma) {
                    break;
                }
            }
        }
        self.consume(TokenType::RightParen, "Expect ')' after arguments.");
        arg_count
    }

    fn variable(&mut self, can_assign: bool) {
        self.named_variable(self.previous.lexeme, can_assign);
    }

    fn named_variable(&mut self, name: &str, can_assign: bool) {
        let (pos, get_op, set_op) = if let Some((pos, depth)) = self.compiler.resolve_local(name) {
            if depth == UNINITIALIZED_LOCAL_DEPTH {
                self.error("Can't read local variable in its own initializer.");
                return;
            }
            (pos, OpCode::GetLocal, OpCode::SetLocal)
        } else if let Some((pos, _)) = self.compiler.resolve_upvalue(name) {
            (pos, OpCode::GetUpvalue, OpCode::SetUpvalue)
        } else {
            let pos = self.identifier_constant(name);
            (pos, OpCode::GetGlobal, OpCode::SetGlobal)
        };

        if can_assign && self._match(TokenType::Equal) {
            // println!("{name} {pos}, {:?}", set_op);
            self.expression();
            self.emit_bytes(set_op, pos as u8);
        } else {
            // println!("{name} {pos}, {:?}", get_op);
            self.emit_bytes(get_op, pos as u8);
        }
    }

    fn and(&mut self, _can_assign: bool) {
        let end_jump = self.emit_jump(OpCode::JumpIfFalse);
        self.emit_byte(OpCode::Pop);
        self.parse_precedence(Precedence::And);
        self.patch_jump(end_jump);
    }

    fn or(&mut self, _can_assign: bool) {
        let else_jump = self.emit_jump(OpCode::JumpIfFalse);
        let end_jump = self.emit_jump(OpCode::Jump);

        self.patch_jump(else_jump);
        self.emit_byte(OpCode::Pop);

        self.parse_precedence(Precedence::Or);
        self.patch_jump(end_jump);
    }

    fn error_at_current(&mut self, message: &str) {
        self.error_at(self.current.clone(), message);
    }

    fn error(&mut self, message: &str) {
        self.error_at(self.previous.clone(), message);
    }

    fn error_at(&mut self, token: Token<'a>, message: &str) {
        if self.panic_mode {
            return;
        }
        self.panic_mode = true;
        eprintln!("[line {}] Error: {}", token.line, message);
        if token.kind == TokenType::Eof {
            eprintln!(" at end");
        } else if token.kind == TokenType::Error {
            // Do nothing.
        } else {
            eprintln!(" at '{}'", token.lexeme);
        }
        eprintln!("{message}");
        self.had_error = true;
    }
}

impl<'a> Compiler<'a> {
    pub fn new(fn_type: FunctionType, name: &str) -> Box<Self> {
        Box::new(Compiler {
            enclosing: None,
            function: Function::new(name, 0),
            fn_type,
            locals: array::from_fn(|i| {
                // Remember that the compiler’s locals array keeps track of which stack slots
                // are associated with which local variables or temporaries.
                // From now on, the compiler implicitly claims stack slot zero for the VM’s own
                //  internal use. We give it an empty name so that the user can’t write an
                // identifier that refers to it.
                if i == 0 {
                    Local {
                        name: Token::default(),
                        depth: 0,
                        is_captured: false,
                    }
                } else {
                    Local::default()
                }
            }),
            // The initial value of the local_count starts at 1
            // because we reserve slot zero for VM use.
            local_count: 1,
            scope_depth: 0,
            upvalues: [Upvalue::default(); MAX_LOCAL_SIZE],
        })
    }

    // Resolve a local variable by name, return its index and depth.
    fn resolve_local(&mut self, name: &str) -> Option<(usize, isize)> {
        (0..self.local_count)
            .rev()
            .find(|&i| self.locals[i].name.lexeme == name)
            .map(|i| (i, self.locals[i].depth))
    }

    fn resolve_upvalue(&mut self, name: &str) -> Option<(usize, isize)> {
        if let Some((index, depth)) = self
            .enclosing
            .as_mut()
            .and_then(|enclosing| enclosing.resolve_local(name))
        {
            if let Some(enclosing) = self.enclosing.as_mut() {
                // When resolving an identifier, if we end up creating an upvalue for
                // a local variable, we mark it as captured.
                enclosing.locals[index].is_captured = true;
            }
            let index = self.add_upvalue(index, true);
            // println!(
            //     "resolve_upvalue: {} {name} {index}, local",
            //     self.function.name
            // );
            return Some((index, depth));
        } else if let Some((index, depth)) = self
            .enclosing
            .as_mut()
            .and_then(|enclosing| enclosing.resolve_upvalue(name))
        {
            let index = self.add_upvalue(index, false);
            // println!(
            //     "resolve_upvalue: {} {name} {index}, upvalue",
            //     self.function.name
            // );
            return Some((index, depth));
        }

        None
    }

    fn add_upvalue(&mut self, index: usize, is_local: bool) -> usize {
        let upvalue_index = self.function.upvalue_count as usize;

        // before we add a new upvalue, we first check to see if the function
        //  already has an upvalue that closes over that variable.
        if let Some(i) = self
            .upvalues
            .iter()
            .take(upvalue_index)
            .position(|u| u.index == index && u.is_local == is_local)
        {
            return i;
        }

        if upvalue_index == MAX_LOCAL_SIZE {
            panic!("Too many closure variables in function.");
        }

        self.upvalues[upvalue_index] = Upvalue { index, is_local };
        self.function.upvalue_count += 1;
        // println!("add upvalue to {upvalue_index} of {:?}", Upvalue { index, is_local });
        upvalue_index
    }
}

impl<'a> ParseRule<'a> {
    fn new(
        prefix: Option<ParseFn<'a>>,
        infix: Option<ParseFn<'a>>,
        precedence: Precedence,
    ) -> Self {
        ParseRule {
            prefix,
            infix,
            precedence,
        }
    }

    fn get_rule(kind: TokenType) -> Self {
        match kind {
            TokenType::LeftParen => {
                Self::new(Some(Parser::grouping), Some(Parser::call), Precedence::Call)
            }
            TokenType::RightParen => Self::new(None, None, Precedence::None),
            TokenType::LeftBrace => Self::new(None, None, Precedence::None),
            TokenType::RightBrace => Self::new(None, None, Precedence::None),
            TokenType::Comma => Self::new(None, None, Precedence::None),
            TokenType::Dot => Self::new(None, None, Precedence::None),
            TokenType::Minus => {
                Self::new(Some(Parser::unary), Some(Parser::binary), Precedence::Term)
            }
            TokenType::Plus => Self::new(None, Some(Parser::binary), Precedence::Term),
            TokenType::Semicolon => Self::new(None, None, Precedence::None),
            TokenType::Slash => Self::new(None, Some(Parser::binary), Precedence::Factor),
            TokenType::Star => Self::new(None, Some(Parser::binary), Precedence::Factor),
            TokenType::Bang => Self::new(Some(Parser::unary), None, Precedence::None),
            TokenType::BangEqual => Self::new(None, Some(Parser::binary), Precedence::Equality),
            TokenType::Equal => Self::new(None, None, Precedence::None),
            TokenType::EqualEqual => Self::new(None, Some(Parser::binary), Precedence::Equality),
            TokenType::Greater => Self::new(None, Some(Parser::binary), Precedence::Comparison),
            TokenType::GreaterEqual => {
                Self::new(None, Some(Parser::binary), Precedence::Comparison)
            }
            TokenType::Less => Self::new(None, Some(Parser::binary), Precedence::Comparison),
            TokenType::LessEqual => Self::new(None, Some(Parser::binary), Precedence::Comparison),
            TokenType::Identifier => Self::new(Some(Parser::variable), None, Precedence::None),
            TokenType::String => Self::new(Some(Parser::string), None, Precedence::None),
            TokenType::Number => Self::new(Some(Parser::number), None, Precedence::None),
            TokenType::And => Self::new(None, Some(Parser::and), Precedence::And),
            TokenType::Class => Self::new(None, None, Precedence::None),
            TokenType::Else => Self::new(None, None, Precedence::None),
            TokenType::False => Self::new(Some(Parser::literal), None, Precedence::None),
            TokenType::For => Self::new(None, None, Precedence::None),
            TokenType::Fun => Self::new(None, None, Precedence::None),
            TokenType::If => Self::new(None, None, Precedence::None),
            TokenType::Nil => Self::new(Some(Parser::literal), None, Precedence::None),
            TokenType::Or => Self::new(None, Some(Parser::or), Precedence::Or),
            TokenType::Print => Self::new(None, None, Precedence::None),
            TokenType::Return => Self::new(None, None, Precedence::None),
            TokenType::Super => Self::new(None, None, Precedence::None),
            TokenType::This => Self::new(None, None, Precedence::None),
            TokenType::True => Self::new(Some(Parser::literal), None, Precedence::None),
            TokenType::Var => Self::new(None, None, Precedence::None),
            TokenType::While => Self::new(None, None, Precedence::None),
            TokenType::Error => Self::new(None, None, Precedence::None),
            TokenType::Eof => Self::new(None, None, Precedence::None),
        }
    }
}

pub fn compile(source: &str) -> Result<Function, InterpretResult> {
    let mut parser = Parser::new(source);
    parser.compile()
}
