use std::{collections::HashMap, convert::Infallible, str::FromStr, sync::Arc};

use http_body_util::BodyExt;
use hyper::{body::Incoming, server::conn::http1, service::service_fn, Method, Request, Response};
use hyper_util::rt::TokioIo;
use spin_core::wasmtime::component::{types::ComponentItem, Type, Val};
use spin_trigger::{
    anyhow::{self, bail, ensure, Context},
    cli::NoCliArgs,
    RuntimeFactors, Trigger, TriggerApp,
};
use tokio::net::TcpListener;

pub struct RemoteTrigger;

impl<T: RuntimeFactors> Trigger<T> for RemoteTrigger {
    const TYPE: &'static str = "remote";

    type CliArgs = NoCliArgs;

    type InstanceState = ();

    fn new(_cli_args: Self::CliArgs, _app: &spin_trigger::App) -> anyhow::Result<Self> {
        Ok(Self)
    }

    async fn run(self, trigger_app: TriggerApp<Self, T>) -> anyhow::Result<()> {
        let trigger_app = Arc::new(trigger_app);
        let listener = TcpListener::bind(("127.0.0.1", 3500)).await?;
        eprintln!("Listening on {:?}", listener.local_addr());
        loop {
            let trigger_app = trigger_app.clone();
            let (stream, _) = listener.accept().await?;
            let io = TokioIo::new(stream);
            tokio::task::spawn(async move {
                if let Err(err) = http1::Builder::new()
                    .serve_connection(
                        io,
                        service_fn(move |req| {
                            let trigger_app = trigger_app.clone();
                            async move {
                                handle(trigger_app.clone(), req).await.or_else(|err| {
                                    Ok::<_, Infallible>(Response::new(err.to_string()))
                                })
                            }
                        }),
                    )
                    .await
                {
                    eprintln!("Error handling request: {err:?}")
                }
            });
        }
    }
}

async fn handle<T: RuntimeFactors>(
    trigger_app: Arc<TriggerApp<RemoteTrigger, T>>,
    req: Request<Incoming>,
) -> anyhow::Result<Response<String>> {
    let path = req.uri().path().trim_start_matches('/').to_string();

    let is_query = match req.method() {
        &Method::GET => true,
        &Method::POST => false,
        other => bail!("unsupported method {other}"),
    };

    let (component_id, export_path) = match path.split_once('/') {
        Some((next, rest)) => (next, (!rest.is_empty()).then_some(rest)),
        None => (&*path, None),
    };
    if component_id.is_empty() && is_query {
        let triggers = trigger_app.app().triggers_with_type("remote");
        let component_ids = triggers
            .map(|trigger| Ok(trigger.component()?.id().to_string()))
            .collect::<anyhow::Result<Vec<_>>>()?;
        return Ok(Response::new(
            #[allow(clippy::format_collect)]
            component_ids
                .iter()
                .map(|id| format!("/{id}/<export>\n"))
                .collect(),
        ));
    }
    let component = trigger_app.get_component(component_id)?;

    let engine = trigger_app.engine().as_ref();
    if export_path.is_none() && is_query {
        return Ok(Response::new(
            component
                .component_type()
                .exports(engine)
                .flat_map(|(name, item)| match item {
                    ComponentItem::ComponentFunc(_) => vec![format!("/{component_id}/{name}\n")],
                    ComponentItem::ComponentInstance(instance) => instance
                        .exports(engine)
                        .filter_map(|(func_name, item)| match item {
                            ComponentItem::ComponentFunc(_) => {
                                Some(format!("/{component_id}/{name}/{func_name}\n"))
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>(),
                    _ => vec![],
                })
                .collect(),
        ));
    }
    let export_path = export_path.context("invalid path")?;

    ensure!(req.method() == Method::POST, "must POST");

    let (item, export_idx) = match export_path.rsplit_once('/') {
        Some((iface_name, func_name)) => {
            let (_, instance_idx) = component
                .export_index(None, iface_name)
                .context("instance not found")?;
            component.export_index(Some(&instance_idx), func_name)
        }
        None => component.export_index(None, export_path),
    }
    .context("func not found")?;

    let ComponentItem::ComponentFunc(func_type) = item else {
        bail!("unexpected export type {item:?}");
    };

    let mut body = req.into_body().collect().await?.to_bytes();
    if body.is_empty() {
        body = "[]".into();
    }
    let value: serde_json::Value = serde_json::from_slice(&body)?;
    let serde_json::Value::Array(args) = value else {
        bail!("request body must be a JSON array");
    };

    let param_types = func_type.params();
    ensure!(
        args.len() == param_types.len(),
        "expected {} args; got {}",
        param_types.len(),
        args.len()
    );

    let params = param_types
        .zip(args)
        .enumerate()
        .map(|(idx, (param_type, arg))| {
            val_from_json(param_type, arg)
                .with_context(|| format!("error parsing argument {}", idx + 1))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let (instance, mut store) = trigger_app.prepare(component_id)?.instantiate(()).await?;
    let func = instance
        .get_func(&mut store, export_idx)
        .context("func instance not found")?;

    let mut results = func_type
        .results()
        .map(|_| Val::Char('\x7f'))
        .collect::<Vec<_>>();
    func.call_async(&mut store, &params, &mut results).await?;

    let results_values = func_type
        .results()
        .zip(results)
        .map(|(ty, val)| val_to_json(ty, val))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let results_json = serde_json::to_string_pretty(&results_values)?;
    Ok(Response::new(results_json))
}

fn val_from_json(ty: Type, value: serde_json::Value) -> anyhow::Result<Val> {
    use serde_json::Value;
    fn int<T>(value: Value) -> anyhow::Result<T>
    where
        T: TryFrom<i64>,
        <T as TryFrom<i64>>::Error: Into<anyhow::Error>,
        T: TryFrom<u64>,
        <T as TryFrom<u64>>::Error: Into<anyhow::Error>,
        T: FromStr,
        <T as FromStr>::Err: Into<anyhow::Error>,
    {
        if let Some(n) = value.as_i64() {
            n.try_into().map_err(Into::into)
        } else if let Some(n) = value.as_u64() {
            n.try_into().map_err(Into::into)
        } else if let Some(s) = value.as_str() {
            s.parse().map_err(Into::into)
        } else {
            bail!("couldn't parse int from {value:?}");
        }
    }
    Ok(match ty {
        Type::Bool => Val::Bool(value.as_bool().context("expected bool")?),
        Type::S8 => Val::S8(int(value)?),
        Type::U8 => Val::U8(int(value)?),
        Type::S16 => Val::S16(int(value)?),
        Type::U16 => Val::U16(int(value)?),
        Type::S32 => Val::S32(int(value)?),
        Type::U32 => Val::U32(int(value)?),
        Type::S64 => Val::S64(int(value)?),
        Type::U64 => Val::U64(int(value)?),
        Type::Float32 => Val::Float32(value.as_f64().context("expected float")? as f32),
        Type::Float64 => Val::Float64(value.as_f64().context("expected float")?),
        Type::Char => Val::Char({
            let s = value.as_str().context("expected string")?;
            let mut chars = s.chars();
            let c = chars.next().context("expected single char")?;
            ensure!(chars.next().is_none(), "expected single char");
            c
        }),
        Type::String => Val::String(value.as_str().context("expected string")?.to_string()),
        Type::List(list) => Val::List({
            let Value::Array(values) = value else {
                bail!("expected array");
            };
            values
                .into_iter()
                .map(|value| val_from_json(list.ty(), value))
                .collect::<anyhow::Result<_>>()?
        }),
        Type::Record(record) => Val::Record({
            let Value::Object(mut object) = value else {
                bail!("expected object");
            };
            let field_vals = record
                .fields()
                .map(|field| {
                    let name = field.name;
                    let maybe_value = object.remove(name);
                    if let Some(value) = maybe_value {
                        Ok((name.to_string(), val_from_json(field.ty, value)?))
                    } else if let Type::Option(_) = field.ty {
                        Ok((name.to_string(), Val::Option(None)))
                    } else {
                        bail!("missing record field '{name}'");
                    }
                })
                .collect::<anyhow::Result<_>>()?;
            if !object.is_empty() {
                bail!("unexpected record field(s): {object:?}");
            }
            field_vals
        }),
        Type::Tuple(tuple) => Val::Tuple({
            let Value::Array(values) = value else {
                bail!("expected array");
            };
            if tuple.types().len() != values.len() {
                bail!(
                    "expected array of {} items; got {}",
                    tuple.types().len(),
                    values.len()
                );
            }
            tuple
                .types()
                .zip(values)
                .map(|(ty, value)| val_from_json(ty, value))
                .collect::<anyhow::Result<_>>()?
        }),
        Type::Variant(variant) => {
            let Value::Object(object) = value else {
                bail!("expected object");
            };
            ensure!(object.len() == 1, "expected object with exactly one field");
            let (key, value) = object.into_iter().next().unwrap();
            let case = variant
                .cases()
                .find(|case| case.name == key)
                .with_context(|| format!("unknown variant case {key}"))?;
            let payload = case
                .ty
                .map(|ty| val_from_json(ty, value))
                .transpose()?
                .map(Box::new);
            Val::Variant(key, payload)
        }
        Type::Enum(_) => Val::Enum(value.as_str().context("expected string")?.to_string()),
        Type::Option(option) => Val::Option(match value {
            Value::Null => None,
            some => Some(Box::new(val_from_json(option.ty(), some)?)),
        }),
        Type::Result(result) => Val::Result({
            let Value::Object(object) = value else {
                bail!("expected object");
            };
            ensure!(
                object.len() == 1,
                "expected object with exactly one field (ok or error)"
            );
            let (key, value) = object.into_iter().next().unwrap();
            match key.as_str() {
                "ok" => Ok(result
                    .ok()
                    .map(|ty| val_from_json(ty, value))
                    .transpose()?
                    .map(Box::new)),
                "error" => Ok(result
                    .err()
                    .map(|ty| val_from_json(ty, value))
                    .transpose()?
                    .map(Box::new)),
                other => bail!("expected ok or error; got {other:?}"),
            }
        }),
        Type::Flags(_) => Val::Flags({
            let Value::Array(values) = value else {
                bail!("expected array");
            };
            values
                .into_iter()
                .map(|value| {
                    let Value::String(s) = value else {
                        bail!("expected string");
                    };
                    Ok(s)
                })
                .collect::<anyhow::Result<_>>()?
        }),
        Type::Own(_) | Type::Borrow(_) => bail!("resource types not supported"),
    })
}

fn val_to_json(ty: Type, val: Val) -> anyhow::Result<serde_json::Value> {
    use serde_json::Value;
    Ok(match (ty, val) {
        (Type::Bool, Val::Bool(val)) => val.into(),
        (Type::S8, Val::S8(val)) => val.into(),
        (Type::U8, Val::U8(val)) => val.into(),
        (Type::S16, Val::S16(val)) => val.into(),
        (Type::U16, Val::U16(val)) => val.into(),
        (Type::S32, Val::S32(val)) => val.into(),
        (Type::U32, Val::U32(val)) => val.into(),
        (Type::S64, Val::S64(val)) => val.into(),
        (Type::U64, Val::U64(val)) => val.into(),
        (Type::Float32, Val::Float32(val)) => val.into(),
        (Type::Float64, Val::Float64(val)) => val.into(),
        (Type::Char, Val::Char(val)) => val.to_string().into(),
        (Type::String, Val::String(val)) => val.into(),
        (Type::List(ty), Val::List(val)) => val
            .into_iter()
            .map(|val| val_to_json(ty.ty(), val))
            .collect::<anyhow::Result<_>>()?,
        (Type::Record(ty), Val::Record(val)) => Value::Object({
            let mut field_types = ty
                .fields()
                .map(|field| (field.name, field.ty))
                .collect::<HashMap<_, _>>();
            val.into_iter()
                .map(|(key, val)| {
                    let ty = field_types
                        .remove(&*key)
                        .with_context(|| format!("invalid key {key:?}"))?;
                    Ok((key, val_to_json(ty, val)?))
                })
                .collect::<anyhow::Result<_>>()?
        }),
        (Type::Tuple(ty), Val::Tuple(val)) => Value::Array({
            let len = ty.types().len();
            ensure!(
                ty.types().len() == val.len(),
                "expected {len} items; got {}",
                val.len()
            );
            ty.types()
                .zip(val)
                .map(|(ty, val)| val_to_json(ty, val))
                .collect::<anyhow::Result<_>>()?
        }),
        (Type::Variant(ty), Val::Variant(case_name, val)) => Value::Object({
            let case = ty
                .cases()
                .find(|case| case.name == case_name)
                .with_context(|| format!("unknown variant case {case_name:?}"))?;
            let payload = optional_val_to_json(case.ty, val)?;
            serde_json::Map::from_iter([(case_name, payload)])
        }),
        (Type::Enum(_), Val::Enum(val)) => val.into(),
        (Type::Option(ty), Val::Option(val)) => val
            .map(|val| val_to_json(ty.ty(), *val))
            .transpose()?
            .into(),
        (Type::Result(ty), Val::Result(val)) => {
            Value::Object(serde_json::Map::from_iter(match val {
                Ok(val) => [("ok".to_string(), optional_val_to_json(ty.ok(), val)?)],
                Err(val) => [("error".to_string(), optional_val_to_json(ty.err(), val)?)],
            }))
        }
        (Type::Flags(_), Val::Flags(val)) => val.into(),
        (Type::Own(_) | Type::Borrow(_), _) => bail!("resource types not supported"),
        (ty, val) => bail!("val {val:?} doesn't match type {ty:?}"),
    })
}

fn optional_val_to_json(
    ty: Option<Type>,
    val: Option<Box<Val>>,
) -> anyhow::Result<serde_json::Value> {
    Ok(match (ty, val) {
        (None, None) => serde_json::Value::Null,
        (Some(ty), Some(val)) => val_to_json(ty, *val)?,
        (ty, val) => bail!("type val mismatch {ty:?}: {val:?}"),
    })
}
