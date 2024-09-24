use exports::{example::remote::iface, wasu::versioned::handler::Kind};

wit_bindgen::generate!({ generate_all });

struct Component;

impl Guest for Component {
    fn echo(payload: String) -> Result<String, String> {
        Ok(payload)
    }
}

impl iface::Guest for Component {
    fn nullary() -> Result<(), String> {
        Err("I always fail!".into())
    }
}

impl exports::wasu::versioned::handler::Guest for Component {
    fn call(kind: Kind) -> Result<String, String> {
        Ok(match kind {
            Kind::Empty => format!("nothing here..."),
            Kind::Full(chars) => format!("full of {chars:?}"),
        })
    }
}

export!(Component);
