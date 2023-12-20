use std::{
    cell::RefCell,
    collections::HashMap,
    io::{BufRead, BufReader, Read},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use mlua::{FromLua, IntoLua, Lua, Table, ThreadStatus, VmState};
use thiserror::Error;

const DEFAULT_TIMEOUT: u64 = 30;
const K_LOADED: &str = "_LOADED";

#[derive(Debug, Error)]
pub enum LamError {
    #[error("lua error: {0}")]
    Lua(#[from] mlua::Error),
}

type LamResult<T> = Result<T, LamError>;

pub struct Evaluation<R>
where
    R: Read,
{
    pub input: RefCell<BufReader<R>>,
    pub script: String,
    pub state: Arc<Mutex<HashMap<String, StateValue>>>,
    pub timeout: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StateValue {
    None,
    Boolean(bool),
    Number(f64),
    String(String),
}

impl<'lua> IntoLua<'lua> for StateValue {
    fn into_lua(self, lua: &'lua Lua) -> mlua::prelude::LuaResult<mlua::prelude::LuaValue<'lua>> {
        match self {
            StateValue::None => Ok(mlua::Value::Nil),
            StateValue::Boolean(b) => b.into_lua(lua),
            StateValue::Number(n) => n.into_lua(lua),
            StateValue::String(s) => s.into_lua(lua),
        }
    }
}

impl<'lua> FromLua<'lua> for StateValue {
    fn from_lua(
        value: mlua::prelude::LuaValue<'lua>,
        _lua: &'lua Lua,
    ) -> mlua::prelude::LuaResult<Self> {
        if let Some(b) = value.as_boolean() {
            return Ok(StateValue::Boolean(b));
        }
        if let Some(n) = value.as_i64() {
            return Ok(StateValue::Number(n as f64));
        }
        if let Some(n) = value.as_f64() {
            return Ok(StateValue::Number(n));
        }
        if let Some(s) = value.as_str() {
            return Ok(StateValue::String(s.to_string()));
        }
        Ok(StateValue::None)
    }
}

pub struct EvaluationBuilder<R>
where
    R: Read,
{
    pub input: R,
    pub script: String,
    pub state: Option<HashMap<String, StateValue>>,
    pub timeout: Option<u64>,
}

impl<R> EvaluationBuilder<R>
where
    R: Read,
{
    pub fn new<S: AsRef<str>>(input: R, script: S) -> Self {
        Self {
            input,
            script: script.as_ref().to_string(),
            state: None,
            timeout: None,
        }
    }

    pub fn set_timeout(mut self, timeout: u64) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn set_state(mut self, state: HashMap<String, StateValue>) -> Self {
        self.state = Some(state);
        self
    }

    pub fn build(self) -> Evaluation<R> {
        Evaluation {
            input: RefCell::new(BufReader::new(self.input)),
            script: self.script,
            state: Arc::new(Mutex::new(self.state.unwrap_or_default())),
            timeout: self.timeout,
        }
    }
}

#[derive(Debug)]
pub struct EvalResult {
    pub duration: Duration,
    pub result: String,
}

pub fn evaluate<R>(e: &mut Evaluation<R>) -> LamResult<EvalResult>
where
    R: Read,
{
    let start = Instant::now();
    let timeout = e.timeout.unwrap_or(DEFAULT_TIMEOUT) as f32;

    let vm = Lua::new();
    vm.sandbox(true)?;
    vm.set_interrupt(move |_| {
        if start.elapsed().as_secs_f32() > timeout {
            return Ok(VmState::Yield);
        }
        Ok(VmState::Continue)
    });

    let r = vm.scope(|scope| {
        let m = vm.create_table()?;
        m.set("_VERSION", env!("CARGO_PKG_VERSION"))?;

        let read_fn = scope.create_function(|_, f: mlua::Value<'_>| {
            if let Some(f) = f.as_str() {
                if f.starts_with("*a") {
                    // accepts *a or *all
                    let mut buf = Vec::new();
                    e.input.borrow_mut().read_to_end(&mut buf)?;
                    let s = vm.create_string(String::from_utf8(buf).unwrap_or_default())?;
                    return Ok(mlua::Value::String(s));
                }
                if f.starts_with("*l") {
                    // accepts *l or *line
                    let mut r = e.input.borrow_mut();
                    let mut buf = String::new();
                    r.read_line(&mut buf)?;
                    let s = vm.create_string(buf)?;
                    return Ok(mlua::Value::String(s));
                }
                if f.starts_with("*n") {
                    // accepts *n or *number
                    let mut buf = String::new();
                    e.input.borrow_mut().read_to_string(&mut buf)?;
                    return Ok(buf
                        .parse::<f64>()
                        .map(mlua::Value::Number)
                        .unwrap_or(mlua::Value::Nil));
                }
            }

            #[allow(clippy::unused_io_amount)]
            if let Some(i) = f.as_usize() {
                let mut buf = vec![0; i];
                let count = e.input.borrow_mut().read(&mut buf)?;
                buf.truncate(count);
                let s = vm.create_string(String::from_utf8(buf).unwrap_or_default())?;
                return Ok(mlua::Value::String(s));
            }

            let s = format!("unexpected format {f:?}");
            Err(mlua::Error::RuntimeError(s))
        })?;
        m.set("read", read_fn)?;

        let read_unicode_fn = scope.create_function(|_, i: usize| {
            let mut expected_read = i;
            let mut buf = Vec::new();
            let mut byte_buf = vec![0; 1];
            loop {
                if expected_read == 0 {
                    return Ok(String::from_utf8(buf).unwrap_or_default());
                }
                let read_bytes = e.input.borrow_mut().read(&mut byte_buf)?;
                // caveat: buffer is not empty when no bytes are read
                if read_bytes > 0 {
                    buf.extend_from_slice(&byte_buf);
                }
                if read_bytes == 0 {
                    return Ok(String::from_utf8(buf).unwrap_or_default());
                }
                if std::str::from_utf8(&buf).is_ok() {
                    expected_read -= 1;
                }
            }
        })?;
        m.set("read_unicode", read_unicode_fn)?;

        let r_state = e.state.clone();
        let get_fn = vm.create_function(move |vm: &Lua, f: mlua::Value<'_>| {
            if let Some(key) = f.as_str() {
                if let Some(v) = r_state
                    .lock()
                    .expect("failed to acquire lock when get state")
                    .get(key)
                {
                    return v.clone().into_lua(vm);
                }
            }
            Ok(mlua::Value::Nil)
        })?;
        m.set("get", get_fn)?;

        let rw_state = e.state.clone();
        let set_fn = vm.create_function(move |vm: &Lua, (k, v): (String, mlua::Value<'_>)| {
            let mut locked = rw_state
                .lock()
                .expect("failed to acquire lock when set state");
            locked.insert(k, StateValue::from_lua(v, vm)?);
            Ok(())
        })?;
        m.set("set", set_fn)?;

        let loaded = vm.named_registry_value::<Table<'_>>(K_LOADED)?;
        loaded.set("@lam", m)?;
        vm.set_named_registry_value(K_LOADED, loaded)?;

        let co = vm.create_thread(vm.load(&e.script).into_function()?)?;
        loop {
            let res = co.resume::<_, Option<String>>(())?;
            if co.status() != ThreadStatus::Resumable || start.elapsed().as_secs_f32() > timeout {
                let r = EvalResult {
                    duration: start.elapsed(),
                    result: res.unwrap_or(String::new()),
                };
                return Ok(r);
            }
        }
    })?;
    Ok(r)
}

#[cfg(test)]
mod test {
    use std::{collections::HashMap, io::Cursor};

    use crate::{evaluate, EvaluationBuilder, StateValue};

    const TIMEOUT_THRESHOLD: f32 = 0.01;

    #[test]
    fn test_evaluate_infinite_loop() {
        let timeout = 1;

        let input: &[u8] = &[];
        let mut e = EvaluationBuilder::new(input, r#"while true do end"#)
            .set_timeout(timeout)
            .build();
        let res = evaluate(&mut e).unwrap();
        assert_eq!("", res.result);

        let secs = res.duration.as_secs_f32();
        let to = timeout as f32;
        assert!((secs - to) / to < TIMEOUT_THRESHOLD, "timed out {}s", secs);
    }

    #[test]
    fn test_read_all() {
        let input = "lam";
        let mut e = EvaluationBuilder::new(
            Cursor::new(input),
            r#"local m = require('@lam'); return m.read('*a')"#,
        )
        .build();
        let res = evaluate(&mut e).unwrap();
        assert_eq!(input, res.result);
    }

    #[test]
    fn test_read_partial_input() {
        let input = "lam";
        let mut e = EvaluationBuilder::new(
            Cursor::new(input),
            r#"local m = require('@lam'); return m.read(1)"#,
        )
        .build();
        let res = evaluate(&mut e).unwrap();
        assert_eq!("l", res.result);
    }

    #[test]
    fn test_read_more_than_input() {
        let input = "l";
        let mut e = EvaluationBuilder::new(
            Cursor::new(input),
            r#"local m = require('@lam'); return m.read(3)"#,
        )
        .build();
        let res = evaluate(&mut e).unwrap();
        assert_eq!("l", res.result);
    }

    #[test]
    fn test_read_unicode() {
        let input = "你好";
        let mut e = EvaluationBuilder::new(
            Cursor::new(input),
            r#"local m = require('@lam'); return m.read_unicode(1)"#,
        )
        .build();
        let res = evaluate(&mut e).unwrap();
        assert_eq!("你", res.result);
    }

    #[test]
    fn test_read_line() {
        let input = "foo\nbar";
        let mut e = EvaluationBuilder::new(
            Cursor::new(input),
            r#"local m = require('@lam'); m.read('*l'); return m.read('*l')"#,
        )
        .build();
        let res = evaluate(&mut e).unwrap();
        assert_eq!("bar", res.result);
    }

    #[test]
    fn test_read_number() {
        let input = "3.1415926";
        let mut e = EvaluationBuilder::new(
            Cursor::new(input),
            r#"local m = require('@lam'); return m.read('*n')"#,
        )
        .build();
        let res = evaluate(&mut e).unwrap();
        assert_eq!("3.1415926", res.result);
    }

    #[test]
    fn test_read_integer() {
        let input = "3";
        let mut e = EvaluationBuilder::new(
            Cursor::new(input),
            r#"local m = require('@lam'); return m.read('*n')"#,
        )
        .build();
        let res = evaluate(&mut e).unwrap();
        assert_eq!("3", res.result);
    }

    #[test]
    fn test_reevaluate() {
        let input = "foo\nbar";

        let mut e = EvaluationBuilder::new(
            Cursor::new(input),
            r#"local m = require('@lam'); return m.read('*l')"#,
        )
        .build();

        let res = evaluate(&mut e).unwrap();
        assert_eq!("foo\n", res.result);

        let res = evaluate(&mut e).unwrap();
        assert_eq!("bar", res.result);
    }

    #[test]
    fn test_handle_binary() {
        let input: &[u8] = &[1, 2, 3];
        let mut e = EvaluationBuilder::new(
            input,
            r#"local m = require('@lam'); local a = m.read('*a'); return #a"#,
        )
        .build();
        let res = evaluate(&mut e).unwrap();
        assert_eq!("3", res.result);
    }

    #[test]
    fn test_state() {
        let input: &[u8] = &[];

        let mut state = HashMap::new();
        state.insert("a".to_string(), StateValue::Number(1.23));

        let mut e = EvaluationBuilder::new(
            input,
            r#"local m = require('@lam'); local a = m.get('a'); m.set('a', 4.56); return a"#,
        )
        .set_state(state)
        .build();

        let res = evaluate(&mut e).unwrap();
        assert_eq!("1.23", res.result);

        let s = e.state.lock().unwrap();
        assert_eq!(&StateValue::Number(4.56), s.get("a").unwrap());
    }

    #[test]
    fn test_reuse_state() {
        let input: &[u8] = &[];

        let mut state = HashMap::new();
        state.insert("a".to_string(), StateValue::Number(1f64));

        let mut e = EvaluationBuilder::new(
            input,
            r#"local m = require('@lam'); local a = m.get('a'); m.set('a', a+1); return a"#,
        )
        .set_state(state)
        .build();

        {
            let res = evaluate(&mut e).unwrap();
            assert_eq!("1", res.result);

            let s = e.state.lock().unwrap();
            assert_eq!(&StateValue::Number(2f64), s.get("a").unwrap());
        }

        {
            let res = evaluate(&mut e).unwrap();
            assert_eq!("2", res.result);

            let s = e.state.lock().unwrap();
            assert_eq!(&StateValue::Number(3f64), s.get("a").unwrap());
        }
    }
}
