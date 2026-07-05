//! wasmtime host for operator-supplied pricing modules (ADR 0003).
//!
//! Sandbox properties:
//! - **Pure**: the linker defines no imports — a module cannot reach the
//!   host, the network, or a clock. All inputs arrive in the request.
//! - **Fuel-metered**: runaway loops trap when fuel runs out.
//! - **Memory-capped**: a store limiter rejects growth past 16 MiB.
//!
//! ABI (documented in `wit/pricing.wit`): the guest exports `memory`,
//! `alloc(len) -> ptr` and `price(ptr, len) -> packed`, where `packed` is
//! `(response_ptr << 32) | response_len`. Request and response are the
//! JSON encodings of `rules::PriceRequest` / `rules::PriceResponse`.

use wasmtime::{Config, Engine, Instance, Module, Store, StoreLimits, StoreLimitsBuilder};

use crate::rules::{FareRuleSet, PriceRequest, PriceResponse, Quote, RuleInput};
use crate::{PricingEngine, PricingError};

const FUEL_PER_CALL: u64 = 100_000_000;
const MAX_MEMORY_BYTES: usize = 16 * 1024 * 1024;

pub struct WasmEngine {
    engine: Engine,
    module: Module,
}

struct HostState {
    limits: StoreLimits,
}

impl WasmEngine {
    /// Compile a pricing module from raw `.wasm` bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, PricingError> {
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine =
            Engine::new(&config).map_err(|e| PricingError::Module(format!("engine: {e}")))?;
        let module = Module::new(&engine, bytes)
            .map_err(|e| PricingError::Module(format!("compile: {e}")))?;
        Ok(Self { engine, module })
    }

    pub fn from_file(path: &std::path::Path) -> Result<Self, PricingError> {
        let bytes = std::fs::read(path)
            .map_err(|e| PricingError::Module(format!("read {}: {e}", path.display())))?;
        Self::from_bytes(&bytes)
    }

    fn call(&self, request: &PriceRequest) -> Result<PriceResponse, PricingError> {
        let module_err = |what: &str| {
            let what = what.to_string();
            move |e: wasmtime::Error| PricingError::Module(format!("{what}: {e}"))
        };

        let mut store = Store::new(
            &self.engine,
            HostState {
                limits: StoreLimitsBuilder::new()
                    .memory_size(MAX_MEMORY_BYTES)
                    .build(),
            },
        );
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(FUEL_PER_CALL)
            .map_err(module_err("set_fuel"))?;

        // No imports: instantiation fails if the module expects any host
        // capability — purity enforced structurally.
        let instance = Instance::new(&mut store, &self.module, &[])
            .map_err(module_err("instantiate (module must not import anything)"))?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| PricingError::Module("module must export `memory`".into()))?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .map_err(module_err("export `alloc`"))?;
        let price = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, "price")
            .map_err(module_err("export `price`"))?;

        let request_bytes = serde_json::to_vec(request)
            .map_err(|e| PricingError::Module(format!("encode request: {e}")))?;
        let ptr = alloc
            .call(&mut store, request_bytes.len() as i32)
            .map_err(module_err("alloc call"))?;
        memory
            .write(&mut store, ptr as usize, &request_bytes)
            .map_err(|e| PricingError::Module(format!("write request: {e}")))?;

        let packed = price
            .call(&mut store, (ptr, request_bytes.len() as i32))
            .map_err(module_err("price call (trap/out of fuel)"))?;
        let response_ptr = ((packed as u64) >> 32) as usize;
        let response_len = ((packed as u64) & 0xFFFF_FFFF) as usize;

        let data = memory.data(&store);
        let response_bytes = data
            .get(response_ptr..response_ptr + response_len)
            .ok_or_else(|| PricingError::Module("response out of bounds".into()))?;
        serde_json::from_slice(response_bytes)
            .map_err(|e| PricingError::Module(format!("decode response: {e}")))
    }
}

impl PricingEngine for WasmEngine {
    fn price(&self, rules: &FareRuleSet, input: &RuleInput) -> Result<Quote, PricingError> {
        let response = self.call(&PriceRequest {
            rules: rules.clone(),
            input: input.clone(),
        })?;
        match (response.ok, response.err) {
            (Some(quote), _) => Ok(quote),
            (None, Some(err)) => Err(PricingError::Module(err)),
            (None, None) => Err(PricingError::Module("empty response".into())),
        }
    }
}
