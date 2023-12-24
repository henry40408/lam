use std::{
    io::{BufRead as _, BufReader, Read},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use dashmap::DashMap;
use mlua::{FromLua, IntoLua, Lua, Table, ThreadStatus, UserData, VmState};
use thiserror::Error;
use tracing::error;

const DEFAULT_TIMEOUT: u64 = 30;
const K_LOADED: &str = "_LOADED";

#[derive(Debug, Error)]
pub enum LamError {
    #[error("lua error: {0}")]
    Lua(#[from] mlua::Error),
}

pub type LamInput<R> = Arc<Mutex<BufReader<R>>>;
pub type LamResult<T> = Result<T, LamError>;
pub type LamStore = Arc<DashMap<String, LamValue>>;

pub struct Evaluation<R>
where
    for<'lua> R: Read + 'lua,
{
    pub vm: mlua::Lua,
    pub input: LamInput<R>,
    pub script: String,
    pub store: LamStore,
    pub timeout: u64,
}

pub struct LuaLam<R>
where
    R: Read,
{
    input: LamInput<R>,
    store: LamStore,
}

impl<R> UserData for LuaLam<R>
where
    R: Read,
{
    fn add_fields<'lua, F: mlua::prelude::LuaUserDataFields<'lua, Self>>(fields: &mut F) {
        fields.add_field("_VERSION", env!("CARGO_PKG_VERSION"));
    }

    fn add_methods<'lua, M: mlua::prelude::LuaUserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("read", |vm, this, f: mlua::Value<'lua>| {
            let mut input = this.input.lock().expect("failed to lock input for read");
            if let Some(f) = f.as_str() {
                if f == "*a" || f == "*all" {
                    // accepts *a or *all
                    let mut buf = Vec::new();
                    let count = input.read_to_end(&mut buf)?;
                    if count == 0 {
                        return Ok(mlua::Value::Nil);
                    }
                    let s = String::from_utf8(buf).unwrap_or_default();
                    return Ok(mlua::Value::String(vm.create_string(s)?));
                }
                if f == "*l" || f == "*line" {
                    // accepts *l or *line
                    let mut buf = String::new();
                    let count = input.read_line(&mut buf)?;
                    if count == 0 {
                        return Ok(mlua::Value::Nil);
                    }
                    return Ok(mlua::Value::String(vm.create_string(buf)?));
                }
                if f == "*n" || f == "*number" {
                    // accepts *n or *number
                    let mut buf = String::new();
                    let count = input.read_to_string(&mut buf)?;
                    if count == 0 {
                        return Ok(mlua::Value::Nil);
                    }
                    return Ok(buf
                        .parse::<f64>()
                        .map(mlua::Value::Number)
                        .unwrap_or(mlua::Value::Nil));
                }
            }

            if let Some(i) = f.as_usize() {
                let mut buf = vec![0; i];
                let count = input.read(&mut buf)?;
                if count == 0 {
                    return Ok(mlua::Value::Nil);
                }
                buf.truncate(count);
                let s = vm.create_string(buf)?;
                return Ok(mlua::Value::String(s));
            }

            let s = format!("unexpected format {f:?}");
            Err(mlua::Error::RuntimeError(s))
        });

        methods.add_method("read_unicode", |_, this, i: u64| {
            let mut input = this
                .input
                .lock()
                .expect("failed to lock input for read_unicode");
            let mut expected_read = i;
            let mut buf = Vec::new();
            let mut byte_buf = vec![0; 1];
            loop {
                if expected_read == 0 {
                    let s = String::from_utf8(buf).unwrap_or_default();
                    return Ok(Some(s));
                }
                let read_bytes = input.read(&mut byte_buf)?;
                if read_bytes == 0 {
                    if buf.is_empty() {
                        return Ok(None);
                    }
                    let s = String::from_utf8(buf).unwrap_or_default();
                    return Ok(Some(s));
                }
                if read_bytes > 0 {
                    buf.extend_from_slice(&byte_buf);
                }
                if std::str::from_utf8(&buf).is_ok() {
                    expected_read -= 1;
                }
            }
        });

        methods.add_method("get", |vm, this, key: String| {
            if let Some(v) = this.store.get(key.as_str()) {
                return v.clone().into_lua(vm);
            }
            Ok(mlua::Value::Nil)
        });

        methods.add_method(
            "set",
            |vm, this, (key, value): (String, mlua::Value<'lua>)| {
                let v = LamValue::from_lua(value.clone(), vm)?;
                this.store.insert(key, v);
                Ok(value)
            },
        );

        methods.add_method(
            "get_set",
            |vm, this, (key, f, default_v): (String, mlua::Function<'lua>, mlua::Value<'lua>)| {
                Ok(this
                    .store
                    .entry(key)
                    .and_modify(|v| match f.call(v.clone().into_lua(vm)) {
                        Ok(ret_v) => match LamValue::from_lua(ret_v, vm) {
                            Ok(store_v) => {
                                *v = store_v;
                            }
                            Err(e) => {
                                error!("failed to convert lua value {:?}", e);
                            }
                        },
                        Err(e) => {
                            error!("failed to run lua function {:?}", e);
                        }
                    })
                    .or_insert(LamValue::from_lua(default_v, vm)?)
                    .value()
                    .clone())
            },
        );
    }
}

impl<R> LuaLam<R>
where
    R: Read,
{
    pub fn new(input: LamInput<R>, store: LamStore) -> Self {
        Self { input, store }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum LamValue {
    None,
    Boolean(bool),
    Number(f64), // represent float and integer
    String(String),
}

impl<'lua> IntoLua<'lua> for LamValue {
    fn into_lua(self, lua: &'lua Lua) -> mlua::prelude::LuaResult<mlua::prelude::LuaValue<'lua>> {
        match self {
            Self::None => Ok(mlua::Value::Nil),
            Self::Boolean(b) => b.into_lua(lua),
            Self::Number(n) => n.into_lua(lua),
            Self::String(s) => s.into_lua(lua),
        }
    }
}

impl<'lua> FromLua<'lua> for LamValue {
    fn from_lua(
        value: mlua::prelude::LuaValue<'lua>,
        _lua: &'lua Lua,
    ) -> mlua::prelude::LuaResult<Self> {
        if let Some(b) = value.as_boolean() {
            return Ok(Self::Boolean(b));
        }
        if let Some(n) = value.as_i64() {
            return Ok(Self::Number(n as f64));
        }
        if let Some(n) = value.as_f64() {
            return Ok(Self::Number(n));
        }
        if let Some(s) = value.as_str() {
            return Ok(Self::String(s.to_string()));
        }
        Ok(Self::None)
    }
}

pub struct EvalBuilder<R>
where
    R: Read,
{
    pub input: R,
    pub script: String,
    pub store: Option<LamStore>,
    pub timeout: Option<u64>,
}

impl<R> EvalBuilder<R>
where
    R: Read,
{
    pub fn new<S: AsRef<str>>(input: R, script: S) -> Self {
        Self {
            input,
            script: script.as_ref().to_string(),
            store: None,
            timeout: None,
        }
    }

    pub fn set_timeout(mut self, timeout: u64) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn set_store(mut self, store: LamStore) -> Self {
        self.store = Some(store);
        self
    }

    pub fn build(self) -> LamResult<Evaluation<R>>
    where
        for<'lua> R: 'lua,
    {
        let vm = mlua::Lua::new();
        vm.sandbox(true)?;
        Ok(Evaluation {
            vm,
            input: Arc::new(Mutex::new(BufReader::new(self.input))),
            script: self.script,
            store: self.store.unwrap_or_default(),
            timeout: self.timeout.unwrap_or(DEFAULT_TIMEOUT),
        })
    }
}

#[derive(Debug)]
pub struct EvalResult {
    pub duration: Duration,
    pub result: String,
}

pub fn evaluate<R>(e: &Evaluation<R>) -> LamResult<EvalResult>
where
    for<'lua> R: Read + 'lua,
{
    let vm = &e.vm;

    let start = Instant::now();
    let timeout = e.timeout as f64;
    vm.set_interrupt(move |_| {
        if start.elapsed().as_secs_f64() > timeout {
            return Ok(VmState::Yield);
        }
        Ok(VmState::Continue)
    });

    let r = vm.scope(|_| {
        let loaded = vm.named_registry_value::<Table<'_>>(K_LOADED)?;

        let lua_lam = LuaLam::new(e.input.clone(), e.store.clone());
        loaded.set("@lam", lua_lam)?;

        vm.set_named_registry_value(K_LOADED, loaded)?;

        let co = vm.create_thread(vm.load(&e.script).into_function()?)?;
        loop {
            let res = co.resume::<_, Option<String>>(())?;
            if co.status() != ThreadStatus::Resumable
                || start.elapsed().as_secs_f64() > e.timeout as f64
            {
                return Ok(EvalResult {
                    duration: start.elapsed(),
                    result: res.unwrap_or(String::new()),
                });
            }
        }
    })?;
    Ok(r)
}

#[cfg(test)]
mod test {
    use std::{io::Cursor, sync::Arc, thread};

    use dashmap::DashMap;

    use crate::{evaluate, EvalBuilder, LamValue};

    const TIMEOUT_THRESHOLD: f32 = 0.01;

    #[test]
    fn test_evaluate_infinite_loop() {
        let timeout = 1;

        let input: &[u8] = &[];
        let e = EvalBuilder::new(input, r#"while true do end"#)
            .set_timeout(timeout)
            .build()
            .unwrap();
        let res = evaluate(&e).unwrap();
        assert_eq!("", res.result);

        let secs = res.duration.as_secs_f32();
        let to = timeout as f32;
        assert!((secs - to) / to < TIMEOUT_THRESHOLD, "timed out {}s", secs);
    }

    #[test]
    fn test_read_all() {
        let input = "lam";
        let e = EvalBuilder::new(Cursor::new(input), r#"return require('@lam'):read('*a')"#)
            .build()
            .unwrap();
        let res = evaluate(&e).unwrap();
        assert_eq!(input, res.result);
    }

    #[test]
    fn test_read_n() {
        let input = "lam";
        {
            let e = EvalBuilder::new(Cursor::new(input), r#"return require('@lam'):read(1)"#)
                .build()
                .unwrap();
            let res = evaluate(&e).unwrap();
            assert_eq!("l", res.result);
        }
        {
            let e = EvalBuilder::new(Cursor::new(input), r#"return require('@lam'):read(3)"#)
                .build()
                .unwrap();
            let res = evaluate(&e).unwrap();
            assert_eq!("lam", res.result);
        }
        {
            let e = EvalBuilder::new(Cursor::new(input), r#"return require('@lam'):read(5)"#)
                .build()
                .unwrap();
            let res = evaluate(&e).unwrap();
            assert_eq!("lam", res.result);
        }
    }

    #[test]
    fn test_read_unicode() {
        let input = "你好";
        let e = EvalBuilder::new(
            Cursor::new(input),
            r#"return require('@lam'):read_unicode(1)"#,
        )
        .build()
        .unwrap();
        let res = evaluate(&e).unwrap();
        assert_eq!("你", res.result);
    }

    #[test]
    fn test_read_line() {
        let input = "foo\nbar";
        let e = EvalBuilder::new(Cursor::new(input), r#"return require('@lam'):read('*l')"#)
            .build()
            .unwrap();
        {
            let res = evaluate(&e).unwrap();
            assert_eq!("foo\n", res.result);
        }
        {
            let res = evaluate(&e).unwrap();
            assert_eq!("bar", res.result);
        }
    }

    #[test]
    fn test_read_number() {
        {
            let input = "3.1415926";
            let e = EvalBuilder::new(Cursor::new(input), r#"return require('@lam'):read('*n')"#)
                .build()
                .unwrap();
            let res = evaluate(&e).unwrap();
            assert_eq!("3.1415926", res.result);
        }
        {
            let input = "3";
            let e = EvalBuilder::new(Cursor::new(input), r#"return require('@lam'):read('*n')"#)
                .build()
                .unwrap();
            let res = evaluate(&e).unwrap();
            assert_eq!("3", res.result);
        }
    }

    #[test]
    fn test_read_empty() {
        let input: &[u8] = &[];
        {
            let e = EvalBuilder::new(input, r#"assert(not require('@lam'):read('*a'))"#)
                .build()
                .unwrap();
            let _ = evaluate(&e).unwrap();
        }
        {
            let e = EvalBuilder::new(input, r#"assert(not require('@lam'):read('*l'))"#)
                .build()
                .unwrap();
            let _ = evaluate(&e).unwrap();
        }
        {
            let e = EvalBuilder::new(input, r#"assert(not require('@lam'):read('*n'))"#)
                .build()
                .unwrap();
            let _ = evaluate(&e).unwrap();
        }
        {
            let e = EvalBuilder::new(input, r#"assert(not require('@lam'):read(1))"#)
                .build()
                .unwrap();
            let _ = evaluate(&e).unwrap();
        }
    }

    #[test]
    fn test_store() {
        let input: &[u8] = &[];

        let store = DashMap::new();
        store.insert("a".to_string(), LamValue::Number(1.23));

        let e = EvalBuilder::new(
            input,
            r#"local m = require('@lam'); local a = m:get('a'); m:set('a', 4.56); return a"#,
        )
        .set_store(Arc::new(store))
        .build()
        .unwrap();

        let res = evaluate(&e).unwrap();
        assert_eq!("1.23", res.result);
        assert_eq!(LamValue::Number(4.56), *e.store.get("a").unwrap());
    }

    #[test]
    fn test_reuse_store() {
        let input: &[u8] = &[];

        let store = DashMap::new();
        store.insert("a".to_string(), LamValue::Number(1f64));

        let e = EvalBuilder::new(
            input,
            r#"local m = require('@lam'); local a = m:get('a'); m:set('a', a+1); return a"#,
        )
        .set_store(Arc::new(store))
        .build()
        .unwrap();

        {
            let res = evaluate(&e).unwrap();
            assert_eq!("1", res.result);
            assert_eq!(LamValue::Number(2f64), *e.store.get("a").unwrap());
        }

        {
            let res = evaluate(&e).unwrap();
            assert_eq!("2", res.result);
            assert_eq!(LamValue::Number(3f64), *e.store.get("a").unwrap());
        }
    }

    #[test]
    fn test_store_concurrency() {
        let input: &[u8] = &[];

        let store = Arc::new(DashMap::new());

        let mut threads = vec![];
        for _ in 0..=100 {
            let store = store.clone();
            threads.push(thread::spawn(move || {
                let e = EvalBuilder::new(
                    input,
                    r#"return require('@lam'):get_set('a', function(v) return v+1 end, 0)"#,
                )
                .set_store(store)
                .build()
                .unwrap();
                let _ = evaluate(&e);
            }));
        }
        for t in threads {
            let _ = t.join();
        }
        assert_eq!(LamValue::Number(100f64), *store.get("a").unwrap());
    }
}
