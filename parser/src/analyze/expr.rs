use super::Analyzer;
use crate::arch;
use crate::data::{self, error::Warning, hir::*, lex::ComparisonToken, *};
use crate::intern::InternedStr;

impl Analyzer {
    pub fn parse_expr(&mut self, expr: ast::Expr) -> Expr {
        use ast::ExprType::*;
        match expr.data {
            Literal(lit) => literal(lit, expr.location),
            Id(id) => self.parse_id(id, expr.location),
            Cast(ctype, inner) => {
                let ctype = self.parse_typename(ctype, expr.location);
                self.explicit_cast(*inner, ctype)
            }
            Shift(left, right, direction) => {
                let op = if direction {
                    BinaryOp::Shl
                } else {
                    BinaryOp::Shr
                };
                self.binary_helper(left, right, op, Self::parse_integer_op)
            }
            BitwiseAnd(left, right) => {
                self.binary_helper(left, right, BinaryOp::BitwiseAnd, Self::parse_integer_op)
            }
            BitwiseOr(left, right) => {
                self.binary_helper(left, right, BinaryOp::BitwiseOr, Self::parse_integer_op)
            }
            Xor(left, right) => {
                self.binary_helper(left, right, BinaryOp::Xor, Self::parse_integer_op)
            }
            Compare(left, right, token) => self.relational_expr(*left, *right, token),
            Mul(left, right) => self.binary_helper(left, right, BinaryOp::Mul, Self::mul),
            Div(left, right) => self.binary_helper(left, right, BinaryOp::Div, Self::mul),
            Mod(left, right) => self.binary_helper(left, right, BinaryOp::Mod, Self::mul),
            Assign(lval, rval, token) => self.assignment_expr(*lval, *rval, token, expr.location),
            Add(left, right) => self.binary_helper(left, right, BinaryOp::Add, Self::add),
            Sub(left, right) => self.binary_helper(left, right, BinaryOp::Sub, Self::add),
            _ => unimplemented!(),
        }
    }
    // only meant for use with `parse_expr`
    // TODO: change ast::Expr to use `ExprType::Binary` as well
    #[allow(clippy::boxed_local)]
    fn binary_helper<F>(
        &mut self, left: Box<ast::Expr>, right: Box<ast::Expr>, op: BinaryOp, expr_checker: F,
    ) -> Expr
    where
        F: FnOnce(&mut Self, Expr, Expr, BinaryOp) -> Expr,
    {
        let func = |a, b, this: &mut Self| expr_checker(this, a, b, op);
        self.parse_binary(*left, *right, func)
    }
    fn parse_binary<F>(&mut self, left: ast::Expr, right: ast::Expr, f: F) -> Expr
    where
        F: FnOnce(Expr, Expr, &mut Self /*, Location*/) -> Expr,
    {
        let left = self.parse_expr(left);
        let right = self.parse_expr(right);
        f(left, right, self)
    }
    fn parse_integer_op(&mut self, left: Expr, right: Expr, op: BinaryOp) -> Expr {
        let non_scalar = if !left.ctype.is_integral() {
            Some(&left.ctype)
        } else if !right.ctype.is_integral() {
            Some(&right.ctype)
        } else {
            None
        };
        let location = left.location.merge(right.location);
        if let Some(ctype) = non_scalar {
            self.err(SemanticError::NonIntegralExpr(ctype.clone()), location);
        }
        let (promoted_expr, next) = Expr::binary_promote(left, right, &mut self.error_handler);
        Expr {
            ctype: next.ctype.clone(),
            expr: ExprType::Binary(op, Box::new(promoted_expr), Box::new(next)),
            lval: false,
            location,
        }
    }
    fn parse_id(&mut self, name: InternedStr, location: Location) -> Expr {
        let pretend_zero = Expr::zero(location);
        match self.scope.get(&name) {
            None => {
                self.err(SemanticError::UndeclaredVar(name), location);
                pretend_zero
            }
            Some(&symbol) => {
                let meta = symbol.get();
                if meta.storage_class == StorageClass::Typedef {
                    self.err(SemanticError::TypedefInExpressionContext, location);
                    return pretend_zero;
                }
                if let Type::Enum(ident, members) = &meta.ctype {
                    let mapper = |(member, value): &(InternedStr, i64)| {
                        if name == *member {
                            Some(*value)
                        } else {
                            None
                        }
                    };
                    let enumerator = members.iter().find_map(mapper);
                    if let Some(e) = enumerator {
                        return Expr {
                            ctype: Type::Enum(*ident, members.clone()),
                            location,
                            lval: false,
                            expr: ExprType::Literal(Literal::Int(e)),
                        };
                    }
                }
                Expr::id(symbol, location)
            }
        }
    }
    fn relational_expr(
        &mut self, left: ast::Expr, right: ast::Expr, token: ComparisonToken,
    ) -> Expr {
        let location = left.location.merge(right.location);
        let mut left = self.parse_expr(left);
        let mut right = self.parse_expr(right);

        if left.ctype.is_arithmetic() && right.ctype.is_arithmetic() {
            let tmp = Expr::binary_promote(left, right, &mut self.error_handler);
            left = tmp.0;
            right = tmp.1;
        } else {
            let (left_expr, right_expr) = (left.rval(), right.rval());
            if !((left_expr.ctype.is_pointer() && left_expr.ctype == right_expr.ctype)
                // equality operations have different rules :(
                || ((token == ComparisonToken::EqualEqual || token == ComparisonToken::NotEqual)
                    // shoot me now
                    && ((left_expr.ctype.is_pointer() && right_expr.ctype.is_void_pointer())
                        || (left_expr.ctype.is_void_pointer() && right_expr.ctype.is_pointer())
                        || (left_expr.is_null() && right_expr.ctype.is_pointer())
                        || (left_expr.ctype.is_pointer() && right_expr.is_null()))))
            {
                self.err(
                    SemanticError::InvalidRelationalType(
                        token,
                        left_expr.ctype.clone(),
                        right_expr.ctype.clone(),
                    ),
                    location,
                );
            }
            left = left_expr;
            right = right_expr;
        }
        assert!(!left.lval && !right.lval);
        Expr {
            lval: false,
            location,
            ctype: Type::Bool,
            expr: ExprType::Binary(BinaryOp::Compare(token), Box::new(left), Box::new(right)),
        }
    }
    fn mul(&mut self, left: Expr, right: Expr, op: BinaryOp) -> Expr {
        let location = left.location.merge(right.location);

        if op == BinaryOp::Mod && !(left.ctype.is_integral() && right.ctype.is_integral()) {
            self.err(
                SemanticError::from(format!(
                    "expected integers for both operators of %, got '{}' and '{}'",
                    left.ctype, right.ctype
                )),
                location,
            );
        } else if !(left.ctype.is_arithmetic() && right.ctype.is_arithmetic()) {
            self.err(
                SemanticError::from(format!(
                    "expected float or integer types for both operands of {}, got '{}' and '{}'",
                    op, left.ctype, right.ctype
                )),
                location,
            );
        }
        let (left, right) = Expr::binary_promote(left, right, &mut self.error_handler);
        Expr {
            ctype: left.ctype.clone(),
            location,
            lval: false,
            expr: ExprType::Binary(op, Box::new(left), Box::new(right)),
        }
    }
    // is_add should be set to `false` if this is a subtraction
    fn add(&mut self, mut left: Expr, mut right: Expr, op: BinaryOp) -> Expr {
        let is_add = op == BinaryOp::Add;
        let location = left.location.merge(right.location);
        match (&left.ctype, &right.ctype) {
            (Type::Pointer(to, _), i)
            | (Type::Array(to, _), i) if i.is_integral() && to.is_complete() => {
                let to = to.clone();
                let (left, right) = (left.rval(), right.rval());
                return self.pointer_arithmetic(left, right, &*to, location);
            }
            (i, Type::Pointer(to, _))
                // `i - p` for pointer p is not valid
            | (i, Type::Array(to, _)) if i.is_integral() && is_add && to.is_complete() => {
                let to = to.clone();
                let (left, right) = (left.rval(), right.rval());
                return self.pointer_arithmetic(right, left, &*to, location);
            }
            _ => {}
        };
        let (ctype, lval) = if left.ctype.is_arithmetic() && right.ctype.is_arithmetic() {
            let tmp = Expr::binary_promote(left, right, &mut self.error_handler);
            left = tmp.0;
            right = tmp.1;
            (left.ctype.clone(), false)
        // `p1 + p2` for pointers p1 and p2 is not valid
        } else if !is_add && left.ctype.is_pointer_to_complete_object() && left.ctype == right.ctype
        {
            // not sure what type to use here, C11 standard doesn't mention it
            (left.ctype.clone(), true)
        } else {
            self.err(
                SemanticError::InvalidAdd(op, left.ctype.clone(), right.ctype.clone()),
                location,
            );
            (left.ctype.clone(), false)
        };
        Expr {
            ctype,
            lval,
            location,
            expr: ExprType::Binary(op, Box::new(left), Box::new(right)),
        }
    }
    fn explicit_cast(&mut self, expr: ast::Expr, ctype: Type) -> Expr {
        let location = expr.location;
        let expr = self.parse_expr(expr);
        if ctype == Type::Void {
            // casting anything to void is allowed
            return Expr {
                lval: false,
                ctype,
                // this just signals to the backend to ignore this outer expr
                expr: ExprType::Cast(Box::new(expr)),
                location,
            };
        }
        if !ctype.is_scalar() {
            self.err(SemanticError::NonScalarCast(ctype.clone()), location);
        } else if expr.ctype.is_floating() && ctype.is_pointer()
            || expr.ctype.is_pointer() && ctype.is_floating()
        {
            self.err(SemanticError::FloatPointerCast(ctype.clone()), location);
        } else if expr.ctype.is_struct() {
            // not implemented: galaga (https://github.com/jyn514/rcc/issues/98)
            self.err(SemanticError::StructCast, location);
        } else if expr.ctype == Type::Void {
            self.err(SemanticError::VoidCast, location);
        }
        Expr {
            lval: false,
            expr: ExprType::Cast(Box::new(expr)),
            ctype,
            location,
        }
    }
    fn pointer_arithmetic(
        &mut self, base: Expr, index: Expr, pointee: &Type, location: Location,
    ) -> Expr {
        let offset = Expr {
            lval: false,
            location: index.location,
            expr: ExprType::Cast(Box::new(index)),
            ctype: base.ctype.clone(),
        }
        .rval();
        let size = match pointee.sizeof() {
            Ok(s) => s,
            Err(_) => {
                self.err(
                    SemanticError::PointerAddUnknownSize(base.ctype.clone()),
                    location,
                );
                1
            }
        };
        let size_literal = literal(Literal::UnsignedInt(size), offset.location);
        let size_cast = Expr {
            lval: false,
            location: offset.location,
            ctype: offset.ctype.clone(),
            expr: ExprType::Cast(Box::new(size_literal)),
        };
        let offset = Expr {
            lval: false,
            location: offset.location,
            ctype: offset.ctype.clone(),
            expr: ExprType::Binary(BinaryOp::Mul, Box::new(size_cast), Box::new(offset)),
        };
        Expr {
            lval: false,
            location,
            ctype: base.ctype.clone(),
            expr: ExprType::Binary(BinaryOp::Add, Box::new(base), Box::new(offset)),
        }
    }
    fn assignment_expr(
        &mut self, lval: ast::Expr, rval: ast::Expr, token: lex::AssignmentToken,
        location: Location,
    ) -> Expr {
        let lval = self.parse_expr(lval);
        let mut rval = self.parse_expr(rval);
        if let Err(err) = lval.modifiable_lval() {
            self.err(err, location);
        }
        if let lex::AssignmentToken::Equal = token {
            if rval.ctype != lval.ctype {
                rval = rval.implicit_cast(&lval.ctype, &mut self.error_handler);
            }
            return Expr {
                ctype: lval.ctype.clone(),
                lval: false, // `(i = j) = 4`; is invalid
                location,
                expr: ExprType::Binary(BinaryOp::Assign, Box::new(lval), Box::new(rval)),
            };
        }
        // Complex assignment is tricky because the left side needs to be evaluated only once
        // Consider e.g. `*f() += 1`: `f()` should only be called once.
        // The hack implemented here is to treat `*f()` as a variable then load and store it to memory:
        // `tmp = *f(); tmp = tmp + 1;`

        // declare tmp in a new hidden scope
        // We really should only be modifying the scope in `FunctionAnalyzer`,
        // but assignment expressions can never appear in an initializer anyway.
        self.scope.enter();
        let tmp_name = InternedStr::get_or_intern("tmp");
        let meta = Metadata {
            id: tmp_name,
            ctype: lval.ctype.clone(),
            qualifiers: Qualifiers::NONE,
            storage_class: StorageClass::Register,
        };
        let ctype = meta.ctype.clone();
        let meta_ref = meta.insert();
        self.scope.insert(tmp_name, meta_ref);
        self.scope.exit();
        // NOTE: this does _not_ call rval() on x
        // tmp = *f()
        let assign = ExprType::Binary(
            BinaryOp::Assign,
            Box::new(Expr {
                ctype: ctype.clone(),
                lval: false,
                location: lval.location,
                expr: ExprType::Id(meta_ref),
            }),
            Box::new(lval),
        );
        // (tmp = *f()), i.e. the expression
        let tmp_assign_expr = Expr {
            expr: assign,
            ctype: ctype.clone(),
            lval: true,
            location,
        };

        // *f() + 1
        let new_val = self.desugar_op(tmp_assign_expr.clone(), rval, token);

        // tmp = *f() + 1
        Expr {
            ctype,
            lval: false,
            location,
            expr: ExprType::Binary(
                BinaryOp::Assign,
                Box::new(tmp_assign_expr),
                Box::new(new_val),
            ),
        }
    }
    fn desugar_op(&mut self, left: Expr, right: Expr, token: lex::AssignmentToken) -> Expr {
        use lex::AssignmentToken::*;

        match token {
            Equal => unreachable!(),
            OrEqual => self.parse_integer_op(left, right, BinaryOp::BitwiseOr),
            AndEqual => self.parse_integer_op(left, right, BinaryOp::BitwiseAnd),
            XorEqual => self.parse_integer_op(left, right, BinaryOp::Xor),
            ShlEqual => self.parse_integer_op(left, right, BinaryOp::Shl),
            ShrEqual => self.parse_integer_op(left, right, BinaryOp::Shr),
            MulEqual => self.mul(left, right, BinaryOp::Mul),
            DivEqual => self.mul(left, right, BinaryOp::Div),
            ModEqual => self.mul(left, right, BinaryOp::Mod),
            AddEqual => self.add(left, right, BinaryOp::Add),
            SubEqual => self.add(left, right, BinaryOp::Sub),
        }
    }
}

// literal
fn literal(literal: Literal, location: Location) -> Expr {
    use crate::data::types::ArrayType;

    let ctype = match &literal {
        Literal::Char(_) => Type::Char(true),
        Literal::Int(_) => Type::Long(true),
        Literal::UnsignedInt(_) => Type::Long(false),
        Literal::Float(_) => Type::Double,
        Literal::Str(s) => {
            let len = s.len() as arch::SIZE_T;
            Type::Array(Box::new(Type::Char(true)), ArrayType::Fixed(len))
        }
    };
    Expr {
        lval: false,
        ctype,
        location,
        expr: ExprType::Literal(literal),
    }
}

fn pointer_promote(left: &mut Expr, right: &mut Expr) -> bool {
    if left.ctype == right.ctype {
        true
    } else if left.ctype.is_void_pointer() || left.ctype.is_char_pointer() || left.is_null() {
        left.ctype = right.ctype.clone();
        true
    } else if right.ctype.is_void_pointer() || right.ctype.is_char_pointer() || right.is_null() {
        right.ctype = left.ctype.clone();
        true
    } else {
        false
    }
}
impl Type {
    #[inline]
    fn is_void_pointer(&self) -> bool {
        match self {
            Type::Pointer(t, _) => **t == Type::Void,
            _ => false,
        }
    }
    #[inline]
    fn is_char_pointer(&self) -> bool {
        match self {
            Type::Pointer(t, _) => match **t {
                Type::Char(_) => true,
                _ => false,
            },
            _ => false,
        }
    }
    #[inline]
    /// used for pointer addition and subtraction, see section 6.5.6 of the C11 standard
    fn is_pointer_to_complete_object(&self) -> bool {
        match self {
            Type::Pointer(ctype, _) => ctype.is_complete() && !ctype.is_function(),
            Type::Array(_, _) => true,
            _ => false,
        }
    }
    /// Return whether self is a signed type.
    ///
    /// Should only be called on integral types.
    /// Calling sign() on a floating or derived type will panic.
    fn sign(&self) -> bool {
        use Type::*;
        match self {
            Char(sign) | Short(sign) | Int(sign) | Long(sign) => *sign,
            Bool => false,
            // TODO: allow enums with values of UINT_MAX
            Enum(_, _) => true,
            x => panic!(
                "Type::sign can only be called on integral types (got {})",
                x
            ),
        }
    }

    /// Return the rank of an integral type, according to section 6.3.1.1 of the C standard.
    ///
    /// It is an error to take the rank of a non-integral type.
    ///
    /// Examples:
    /// ```ignore
    /// use rcc::data::types::Type::*;
    /// assert!(Long(true).rank() > Int(true).rank());
    /// assert!(Int(false).rank() > Short(false).rank());
    /// assert!(Short(true).rank() > Char(true).rank());
    /// assert!(Char(true).rank() > Bool.rank());
    /// assert!(Long(false).rank() > Bool.rank());
    /// assert!(Long(true).rank() == Long(false).rank());
    /// ```
    fn rank(&self) -> usize {
        use Type::*;
        match self {
            Bool => 0,
            Char(_) => 1,
            Short(_) => 2,
            Int(_) => 3,
            Long(_) => 4,
            // don't make this 5 in case we add `long long` at some point
            _ => std::usize::MAX,
        }
    }
    fn integer_promote(self) -> Type {
        if self.rank() <= Type::Int(true).rank() {
            if Type::Int(true).can_represent(&self) {
                Type::Int(true)
            } else {
                Type::Int(false)
            }
        } else {
            self
        }
    }
    fn binary_promote(mut left: Type, mut right: Type) -> Type {
        use Type::*;
        if left == Double || right == Double {
            return Double; // toil and trouble
        } else if left == Float || right == Float {
            return Float;
        }
        left = left.integer_promote();
        right = right.integer_promote();
        let signs = (left.sign(), right.sign());
        // same sign
        if signs.0 == signs.1 {
            return if left.rank() >= right.rank() {
                left
            } else {
                right
            };
        };
        let (signed, unsigned) = if signs.0 {
            (left, right)
        } else {
            (right, left)
        };
        if signed.can_represent(&unsigned) {
            signed
        } else {
            unsigned
        }
    }
    fn is_struct(&self) -> bool {
        match self {
            Type::Struct(_) | Type::Union(_) => true,
            _ => false,
        }
    }
    fn is_complete(&self) -> bool {
        match self {
            Type::Void | Type::Function(_) | Type::Array(_, types::ArrayType::Unbounded) => false,
            // TODO: update when we allow incomplete struct and union types (e.g. `struct s;`)
            _ => true,
        }
    }
}

impl Expr {
    fn zero(location: Location) -> Expr {
        Expr {
            ctype: Type::Int(true),
            expr: ExprType::Literal(Literal::Int(0)),
            lval: false,
            location,
        }
    }
    fn is_null(&self) -> bool {
        if let ExprType::Literal(token) = &self.expr {
            match token {
                Literal::Int(0) | Literal::UnsignedInt(0) | Literal::Char(0) => true,
                _ => false,
            }
        } else {
            false
        }
    }
    fn id(symbol: MetadataRef, location: Location) -> Self {
        Self {
            expr: ExprType::Id(symbol),
            // TODO: maybe pass in the type as well to avoid the lookup?
            // but then we need to make sure the type matches the symbol
            ctype: symbol.get().ctype.clone(),
            lval: true,
            location,
        }
    }
    // Perform a binary conversion, including all relevant casts.
    //
    // See `Type::binary_promote` for conversion rules.
    fn binary_promote(left: Expr, right: Expr, error_handler: &mut ErrorHandler) -> (Expr, Expr) {
        let (left, right) = (left.rval(), right.rval());
        let ctype = Type::binary_promote(left.ctype.clone(), right.ctype.clone());
        (
            left.implicit_cast(&ctype, error_handler),
            right.implicit_cast(&ctype, error_handler),
        )
    }
    // ensure an expression has a value. convert
    // - arrays -> pointers
    // - functions -> pointers
    // - variables -> value stored in that variable
    pub(super) fn rval(self) -> Expr {
        match self.ctype {
            // a + 1 is the same as &a + 1
            Type::Array(to, _) => Expr {
                lval: false,
                ctype: Type::Pointer(to, Qualifiers::default()),
                ..self
            },
            Type::Function(_) => Expr {
                lval: false,
                ctype: Type::Pointer(
                    Box::new(self.ctype),
                    Qualifiers {
                        c_const: true,
                        ..Qualifiers::default()
                    },
                ),
                ..self
            },
            // HACK: structs can't be dereferenced since they're not scalar, so we just fake it
            Type::Struct(_) | Type::Union(_) if self.lval => Expr {
                lval: false,
                ..self
            },
            _ if self.lval => Expr {
                ctype: self.ctype.clone(),
                lval: false,
                location: self.location,
                expr: ExprType::Deref(Box::new(self)),
            },
            _ => self,
        }
    }
    pub(super) fn implicit_cast(mut self, ctype: &Type, error_handler: &mut ErrorHandler) -> Expr {
        if &self.ctype == ctype {
            self
        } else if self.ctype.is_arithmetic() && ctype.is_arithmetic()
            || self.is_null() && ctype.is_pointer()
            || self.ctype.is_pointer() && ctype.is_bool()
            || self.ctype.is_pointer() && ctype.is_void_pointer()
            || self.ctype.is_pointer() && ctype.is_char_pointer()
        {
            Expr {
                location: self.location,
                expr: ExprType::Cast(Box::new(self)),
                lval: false,
                ctype: ctype.clone(),
            }
        } else if ctype.is_pointer()
            && (self.is_null() || self.ctype.is_void_pointer() || self.ctype.is_char_pointer())
        {
            self.ctype = ctype.clone();
            self
        } else if self.ctype == Type::Error {
            self
        // TODO: allow implicit casts of const pointers
        } else {
            error_handler.error(
                SemanticError::InvalidCast(
                    //"cannot implicitly convert '{}' to '{}'{}",
                    self.ctype.clone(),
                    ctype.clone(),
                ),
                self.location,
            );
            self
        }
    }
    /// See section 6.3.2.1 of the C Standard. In particular:
    /// "A modifiable lvalue is an lvalue that does not have array type,
    /// does not  have an incomplete type, does not have a const-qualified type,
    /// and if it is a structure or union, does not have any member with a const-qualified type"
    fn modifiable_lval(&self) -> Result<(), SemanticError> {
        let err = |e| Err(SemanticError::NotAssignable(e));
        // rval
        if !self.lval {
            return err("rvalue".to_string());
        }
        // incomplete type
        if !self.ctype.is_complete() {
            return err(format!("expression with incomplete type '{}'", self.ctype));
        }
        // const-qualified type
        // TODO: handle `*const`
        if let ExprType::Id(sym) = &self.expr {
            let meta = sym.get();
            if meta.qualifiers.c_const {
                return err(format!("variable '{}' with `const` qualifier", meta.id));
            }
        }
        match &self.ctype {
            // array type
            Type::Array(_, _) => err("array".to_string()),
            // member with const-qualified type
            Type::Struct(stype) | Type::Union(stype) => {
                if stype
                    .members()
                    .iter()
                    .map(|sym| sym.qualifiers.c_const)
                    .any(|x| x)
                {
                    err("struct or union with `const` qualified member".to_string())
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::analyze::test::analyze;
    use crate::analyze::*;
    pub(crate) fn parse_expr(input: &str) -> CompileResult<Expr> {
        analyze(input, Parser::expr, Analyzer::parse_expr)
    }
    fn get_location(r: &CompileResult<Expr>) -> Location {
        match r {
            Ok(expr) => expr.location,
            Err(err) => err.location(),
        }
    }
    fn assert_literal(token: Literal) {
        let parsed = parse_expr(&token.to_string());
        let location = get_location(&parsed);
        assert_eq!(parsed.unwrap(), literal(token, location));
    }
    /*
    fn parse_expr_with_scope<'a>(input: &'a str, variables: &[&Symbol]) -> CompileResult<Expr> {
        let mut parser = parser(input);
        let mut scope = Scope::new();
        for var in variables {
            scope.insert(var.id.clone(), (*var).clone());
        }
        parser.scope = scope;
        let exp = parser.expr();
        if let Some(err) = parser.error_handler.pop_front() {
            Err(err)
        } else {
            exp.map_err(CompileError::from)
        }
    }
    */
    fn assert_type(input: &str, ctype: Type) {
        match parse_expr(input) {
            Ok(expr) => assert_eq!(expr.ctype, ctype),
            Err(err) => panic!("error: {}", err.data),
        };
    }
    #[test]
    fn test_primaries() {
        assert_literal(Literal::Int(141));
        let parsed = parse_expr("\"hi there\"");

        /*
        assert_eq!(
            parsed,
            Ok(Expr::from((
                Literal::Str("hi there\0".into()),
                get_location(&parsed)
            )))
        );
        assert_literal(Literal::Float(1.5));
        let parsed = parse_expr("(1)");
        assert_eq!(
            parsed,
            Ok(Expr::from((Literal::Int(1), get_location(&parsed))))
        );
        let x = Symbol {
            ctype: Type::Int(true),
            id: InternedStr::get_or_intern("x"),
            qualifiers: Default::default(),
            storage_class: Default::default(),
            init: false,
        };
        let parsed = parse_expr_with_scope("x", &[&x]);
        assert_eq!(
            parsed,
            Ok(Expr {
                location: get_location(&parsed),
                ctype: Type::Int(true),
                lval: true,
                expr: ExprType::Id(x)
            })
        );
        */
    }
    #[test]
    fn test_mul() {
        assert_type("1*1.0", Type::Double);
        assert_type("1*2.0 / 1.3", Type::Double);
        assert_type("3%2", Type::Long(true));
    }
    /*
    #[test]
    fn test_funcall() {
        let f = Symbol {
            id: InternedStr::get_or_intern("f"),
            init: false,
            qualifiers: Default::default(),
            storage_class: Default::default(),
            ctype: Type::Function(types::FunctionType {
                params: vec![Symbol {
                    ctype: Type::Void,
                    id: Default::default(),
                    init: false,
                    qualifiers: Default::default(),
                    storage_class: StorageClass::Auto,
                }],
                return_type: Box::new(Type::Int(true)),
                varargs: false,
            }),
        };
        assert!(parse_expr_with_scope("f(1,2,3)", &[&f]).is_err());
        let parsed = parse_expr_with_scope("f()", &[&f]);
        assert!(match parsed {
            Ok(Expr {
                expr: ExprType::FuncCall(_, _),
                ..
            }) => true,
            _ => false,
        },);
    }
    */
    #[test]
    fn test_type_errors() {
        assert!(parse_expr("1 % 2.0").is_err());
    }

    #[test]
    fn test_explicit_casts() {
        assert_type("(int)4.2", Type::Int(true));
        assert_type("(unsigned int)4.2", Type::Int(false));
        assert_type("(float)4.2", Type::Float);
        assert_type("(double)4.2", Type::Double);
        assert!(parse_expr("(int*)4.2").is_err());
        assert_type(
            "(int*)(int)4.2",
            Type::Pointer(Box::new(Type::Int(true)), Qualifiers::default()),
        );
    }
}