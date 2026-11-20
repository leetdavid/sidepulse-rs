use wasmi::{Engine, Instance, Linker, Memory, Module, Store, TypedFunc};

const WASM: &[u8] = include_bytes!("../resources/sdled.wasm");
const INPUT_BYTES: usize = 512;

const ERROR_NAMES: &[&str] = &[
    "ok",
    "empty program",
    "program is longer than 512 bytes",
    "too many lines",
    "too many animation lines",
    "syntax error",
    "bad color",
    "bad LED index",
    "bad time",
    "bad brightness",
    "bad repeat",
    "unexpected trailing text",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub line: u8,
    pub column: u8,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (line {}, column {})", self.message, self.line, self.column)
    }
}

fn compiled() -> Result<&'static (Engine, Module), String> {
    static COMPILED: std::sync::OnceLock<Result<(Engine, Module), String>> = std::sync::OnceLock::new();
    COMPILED
        .get_or_init(|| {
            let engine = Engine::default();
            let module = Module::new(&engine, WASM).map_err(|e| e.to_string())?;
            Ok((engine, module))
        })
        .as_ref()
        .map_err(Clone::clone)
}

pub struct LedEngine {
    store: Store<()>,
    memory: Memory,
    input: usize,
    output: usize,
    reset: TypedFunc<(i32, i32), ()>,
    parse: TypedFunc<(i32, i32, i32), i32>,
    step: TypedFunc<(i32, i32), ()>,
    led_count: u8,
}

impl LedEngine {
    pub fn new(led_count: u8) -> Result<Self, String> {
        let (engine, module) = compiled()?;
        let mut store = Store::new(engine, ());
        let instance: Instance = Linker::<()>::new(engine).instantiate_and_start(&mut store, module).map_err(|e| e.to_string())?;
        let memory = instance.get_memory(&store, "memory").ok_or("sdled.wasm exports no memory")?;
        let pointer = |store: &mut Store<()>, name: &str| -> Result<usize, String> {
            let func = instance.get_typed_func::<(), i32>(&*store, name).map_err(|e| e.to_string())?;
            func.call(store, ()).map(|p| p as u32 as usize).map_err(|e| e.to_string())
        };
        let input = pointer(&mut store, "sdled_input_ptr")?;
        let output = pointer(&mut store, "sdled_output_ptr")?;
        let reset = instance.get_typed_func(&store, "sdled_reset").map_err(|e| e.to_string())?;
        let parse = instance.get_typed_func(&store, "sdled_parse").map_err(|e| e.to_string())?;
        let step = instance.get_typed_func(&store, "sdled_step").map_err(|e| e.to_string())?;
        let mut engine = Self { store, memory, input, output, reset, parse, step, led_count: if led_count == 2 { 2 } else { 8 } };
        engine.reset_at(0);
        Ok(engine)
    }

    pub fn reset_at(&mut self, now_ms: u32) {
        let _ = self.reset.call(&mut self.store, (i32::from(self.led_count), now_ms as i32));
    }

    pub fn load(&mut self, program: &str, now_ms: u32) -> Result<(), ParseError> {
        let bytes = program.as_bytes();
        let mut buffer = [0u8; INPUT_BYTES];
        let copied = bytes.len().min(INPUT_BYTES);
        buffer[..copied].copy_from_slice(&bytes[..copied]);
        let _ = self.memory.write(&mut self.store, self.input, &buffer);
        let length = if bytes.len() > INPUT_BYTES { INPUT_BYTES + 1 } else { bytes.len() };
        let packed = self
            .parse
            .call(&mut self.store, (i32::from(self.led_count), length as i32, now_ms as i32))
            .map_err(|e| ParseError { message: e.to_string(), line: 0, column: 0 })? as u32;
        if packed & 1 == 1 {
            return Ok(());
        }
        let error = ((packed >> 8) & 0xff) as usize;
        Err(ParseError {
            message: ERROR_NAMES.get(error).map_or_else(|| format!("error {error}"), |s| (*s).to_string()),
            line: ((packed >> 16) & 0xff) as u8,
            column: ((packed >> 24) & 0xff) as u8,
        })
    }

    pub fn step(&mut self, now_ms: u32) -> Vec<[u8; 3]> {
        let _ = self.step.call(&mut self.store, (i32::from(self.led_count), now_ms as i32));
        let mut pixels = vec![0u8; usize::from(self.led_count) * 3];
        let _ = self.memory.read(&self.store, self.output, &mut pixels);
        pixels.as_chunks::<3>().0.to_vec()
    }
}

pub fn validate(program: &str, led_count: u8) -> Result<(), ParseError> {
    LedEngine::new(led_count).map_err(|message| ParseError { message, line: 0, column: 0 })?.load(program, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solid_color_reaches_target() {
        let mut engine = LedEngine::new(8).unwrap();
        engine.load("#ff0000", 0).unwrap();
        let pixels = engine.step(100);
        assert_eq!(pixels.len(), 8);
        assert!(pixels.iter().all(|p| *p == [255, 0, 0]));
    }

    #[test]
    fn indexed_assignment_only_touches_that_led() {
        let mut engine = LedEngine::new(8).unwrap();
        engine.load("off\n3:#00ff00", 0).unwrap();
        let pixels = engine.step(200);
        assert_eq!(pixels[3], [0, 255, 0]);
        assert_eq!(pixels[0], [0, 0, 0]);
    }

    #[test]
    fn reports_syntax_errors_with_position() {
        let error = validate("#ff00ff 1s\nbogus words", 8).unwrap_err();
        assert_eq!(error.line, 2);
        assert!(validate("#gg0000", 8).is_err());
        assert!(validate("off\n#ff00ff 1.4s pulse\nrepeat", 8).is_ok());
    }

    #[test]
    fn one_shot_lid_animation_lights_then_ends_dark() {
        let mut engine = LedEngine::new(8).unwrap();
        engine.load(&crate::led::builtin("ember-lid-open", 8).unwrap(), 0).unwrap();
        let lit = (0..1200).step_by(50).any(|t| engine.step(t).iter().any(|p| p.iter().any(|c| *c > 64)));
        assert!(lit);
        assert!(engine.step(3000).iter().all(|p| p.iter().all(|c| *c < 8)));
    }

    #[test]
    fn every_builtin_animation_parses() {
        for id in ["amber-pulse", "cyan-roll", "cyan-complete", "idle-pulse", "kitt", "lid-open", "lid-closed", "solid-green", "ember-tide", "purple-idle", "night-rider", "off", "immediate-off"] {
            for count in [2, 8] {
                let program = crate::led::builtin(id, count).unwrap();
                assert!(validate(&program, count).is_ok(), "{id}-{count}: {:?}", validate(&program, count));
            }
        }
    }
}
