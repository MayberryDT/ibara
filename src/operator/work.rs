//! Managed command entry over the same MCP/SSH route as desktop work. Each
//! invocation is a connection; background jobs belong to the durable task.
use super::{LineRead, fail, read_line_bounded, runtime};
use crate::error::Result;
use serde_json::{Value, json};
use std::process::{ExitCode, Stdio};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use sha2::{Digest, Sha256};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::io::Write;

// Persist the acquisition before launching: an uncertain CLI retry must not
// acquire a second task after the first command has already completed.
struct Receipt { path:std::path::PathBuf, data:Value }
impl Receipt {
    fn open(input:&Input)->Result<(Self,bool)> {
        let directory=super::home_dir().join(".local/state/ibara/work");
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&directory)?;
        let key=format!("{:x}",Sha256::digest(format!("{}:{}:{}",input.verb,input.agent,input.args["request_id"]).as_bytes()));
        let fingerprint=format!("{:x}",Sha256::digest(json!({"route":input.route,"agent":input.agent,"args":input.args,"command":input.command}).to_string().as_bytes()));
        let path=directory.join(format!("{key}.json"));
        let data=json!({"fingerprint":fingerprint,"request_id":input.args["request_id"],"phase":if input.verb=="run" {"acquiring"} else {"launching"},
            "work":if input.verb=="exec" {json!({"task_ref":input.args["task_ref"]})} else {Value::Null}});
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
            Ok(mut file)=>{file.write_all(data.to_string().as_bytes())?;file.sync_all()?;Ok((Self{path,data},true))}
            Err(e) if e.kind()==std::io::ErrorKind::AlreadyExists=>{
                let data:Value=serde_json::from_slice(&std::fs::read(&path)?).map_err(|_|fail("Unreadable work receipt; inspect the existing task before retrying."))?;
                if data["fingerprint"]!=fingerprint {return Err(fail("request-id already names different work; use its original arguments or a new ID for intentionally new work."));}
                Ok((Self{path,data},false))
            }
            Err(e)=>Err(e.into())
        }
    }
    fn save(&self)->Result<()> {super::replace_file(&self.path,self.data.to_string().as_bytes(),0o600)?;Ok(())}
}

const USAGE: &str = "Usage: ibara work [--computer NAME] [--directory-db FILE] [--agent NAME] COMMAND\n\
  run --goal TEXT [--request-id ID] [--cwd PATH] [--timeout-ms N] -- PROGRAM ARG...\n\
  exec --task TASK [--request-id ID] [--cwd PATH] [--timeout-ms N] -- PROGRAM ARG...\n\
  status [REF]\n\
  cancel TASK | finish TASK\n\n\
run selects/acquires a computer and launches a managed background job. It prints\n\
task/operation IDs for status and reconnect; connection loss does not cancel jobs.\n\
exec continues that task. Commands default to its private working directory.\n\
No shell interpretation; invoke a shell explicitly if needed. The runtime limit\n\
is returned in the job receipt (exceeding explicit host policy is refused). finish/cancel stop\n\
remaining jobs and clean owned windows. Human takeover also cancels jobs.\n\
Administrative SSH and detached external services are outside job supervision.\n";

struct Input { route:Vec<String>, agent:String, verb:String, args:Value, command:Vec<String> }

fn parse(args:Vec<String>) -> Result<Input> {
    let mut route=Vec::new(); let mut agent="ibara-work".to_string();
    let mut i=0;
    while let Some(key)=args.get(i).filter(|v|v.starts_with("--")) {
        let val=args.get(i+1).ok_or_else(||fail(USAGE))?.clone();
        match key.as_str() {"--computer"|"--directory-db"=>route.extend([key.clone(),val]),"--agent"=>agent=val,_=>return Err(fail(USAGE))}
        i+=2;
    }
    let verb=args.get(i).ok_or_else(||fail(USAGE))?.clone(); i+=1;
    let mut fields=json!({}); let mut command=Vec::new();
    if matches!(verb.as_str(),"run"|"exec") {
        while let Some(key)=args.get(i) {
            if key=="--" {command=args[i+1..].to_vec();break;}
            let val=args.get(i+1).ok_or_else(||fail(USAGE))?;
            let name=match key.as_str() {"--goal"=>"goal","--task"=>"task_ref","--request-id"=>"request_id","--cwd"=>"cwd","--timeout-ms"=>"timeout_ms",_=>return Err(fail(USAGE))};
            if !fields[name].is_null() {return Err(fail("Duplicate work option."));}
            fields[name]=if name=="timeout_ms" {json!(val.parse::<u64>().ok().filter(|n|*n>0).ok_or_else(||fail("timeout-ms must be positive"))?)} else {json!(val)};
            i+=2;
        }
        if command.first().is_none_or(|s|s.is_empty()) || fields[if verb=="run" {"goal"} else {"task_ref"}].as_str().is_none_or(|s|s.is_empty()) {
            return Err(fail(USAGE));
        }
        if (verb=="run" && !fields["task_ref"].is_null()) || (verb=="exec" && !fields["goal"].is_null()) {return Err(fail(USAGE));}
        if fields["request_id"].is_null() {fields["request_id"]=json!(crate::ids::id("work"));}
    } else if matches!(verb.as_str(),"status"|"cancel"|"finish") {
        if let Some(reference)=args.get(i) {fields["ref"]=json!(reference);i+=1;}
        if i!=args.len() || (verb!="status" && fields["ref"].is_null()) {return Err(fail(USAGE));}
    } else {return Err(fail(USAGE));}
    Ok(Input{route,agent,verb,args:fields,command})
}

struct Link { child:Child, input:ChildStdin, output:BufReader<ChildStdout>, id:u64 }
impl Link {
    async fn open(route:&[String],agent:&str)->Result<Self> {
        let mut child=Command::new(std::env::current_exe()?).arg("mcp").args(route)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).kill_on_drop(true).spawn()?;
        let input=child.stdin.take().unwrap(); let output=BufReader::new(child.stdout.take().unwrap());
        let mut link=Self{child,input,output,id:0};
        link.request("initialize",json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":agent,"version":crate::version()}})).await?;
        link.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await?;
        Ok(link)
    }
    async fn send(&mut self,v:Value)->Result<()> { self.input.write_all(format!("{v}\n").as_bytes()).await?;self.input.flush().await?;Ok(()) }
    async fn request(&mut self,method:&str,params:Value)->Result<Value> {
        self.id+=1;let id=self.id;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).await?;
        loop {
            let line=match read_line_bounded(&mut self.output,32*1024*1024,true).await? {LineRead::Line(s)=>s,_=>return Err(fail("Work connection ended. Inspect task/operation status before retrying any effect."))};
            let v:Value=serde_json::from_slice(&line).map_err(|_|fail("Invalid work response"))?;
            if v["method"]=="ping" {self.send(json!({"jsonrpc":"2.0","id":v["id"],"result":{}})).await?;continue;}
            if v["id"]!=id {continue;}
            if !v["error"].is_null() {return Err(fail(format!("{}",v["error"])));}
            return Ok(v["result"].clone());
        }
    }
    async fn call(&mut self,tool:&str,args:Value)->Result<Value> {
        let result=self.request("tools/call",json!({"name":tool,"arguments":args})).await?;
        let envelope=&result["structuredContent"];
        if !envelope.is_object() {return Err(fail("Target did not return a structured work receipt; inspect status before retrying."));}
        Ok(envelope.clone())
    }
    async fn close(mut self) {let _=self.input.shutdown().await;let _=tokio::time::timeout(std::time::Duration::from_secs(6),self.child.wait()).await;}
}

async fn execute(input:Input)->Result<Value> {
    let mut receipt=if matches!(input.verb.as_str(),"run"|"exec") {Some(Receipt::open(&input)?)} else {None};
    if let Some((record,_))=&receipt {
        eprintln!("{}",json!({"request_id":record.data["request_id"],"task_ref":record.data["work"]["task_ref"],"receipt":record.path,
            "next":"Keep these IDs. After interruption inspect status; do not start replacement work."}));
    }
    if let Some((record,false))=&receipt {
        if record.data["work"]["task_ref"].is_null() {
            return Err(fail(format!("Acquisition already attempted; inspect fleet/task state before retrying. Receipt: {}",record.path.display())));
        }
    }
    let mut link=Link::open(&input.route,&input.agent).await?;
    let mut args=input.args;
    let mut acquired=None;
    let result=match input.verb.as_str() {
        "run"|"exec"=>{
            if input.verb=="run" {
                let (record,fresh)=receipt.as_mut().unwrap();
                if !*fresh {
                    let reference=record.data["op_ref"].as_str().or(record.data["work"]["task_ref"].as_str()).unwrap();
                    let mut status=link.call("computer_status",json!({"ref":reference})).await?;
                    status["work"]=record.data["work"].clone();
                    status["next"]=json!("Inspect this work before further effects. Continue with exec --task and the original request ID if its command reply was lost; do not start a replacement task.");
                    link.close().await;return Ok(status);
                }
                let began=link.call("computer_begin",json!({"goal":args["goal"],"request_id":format!("{}-begin",args["request_id"].as_str().unwrap())})).await?;
                if began["status"]!="ok" {record.data["refusal"]=began.clone();record.save()?;link.close().await;return Ok(began);}
                args["task_ref"]=began["result"]["task_ref"].clone();
                let work=json!({"task_ref":began["result"]["task_ref"],"computer":began["result"]["computer"],"workspace":began["result"]["workspace"]});
                record.data["work"]=work.clone();record.data["phase"]=json!("launching");record.save()?;
                eprintln!("{}",json!({"request_id":record.data["request_id"],"work":work,"receipt":record.path}));
                acquired=Some(work);
            }
            args.as_object_mut().unwrap().remove("goal");args["command"]=json!(input.command);args["background"]=json!(true);
            let task=args["task_ref"].clone();
            let mut reply=link.call("computer_exec",args).await?;
            reply["work"]=acquired.unwrap_or_else(||json!({"task_ref":task}));
            if let Some((record,_))=receipt.as_mut() {record.data["op_ref"]=reply["result"]["op_ref"].clone();record.data["phase"]=json!("answered");record.save()?;}
            reply
        }
        "status"=>link.call("computer_status",args).await?,
        _=>link.call("computer_finish",json!({"task_ref":args["ref"],"request_id":crate::ids::id("finish"),
            "outcome":if input.verb=="cancel" {"cancelled"} else {"partial"},
            "summary":if input.verb=="cancel" {"Explicitly cancelled through work CLI"} else {"Work closed through CLI; no unverified success claim"}})).await?,
    };
    link.close().await;Ok(result)
}

pub fn main(args:Vec<String>)->ExitCode {
    if args==["--help"] || args==["help"] {print!("{USAGE}");return ExitCode::SUCCESS;}
    let result=parse(args).and_then(|input|runtime().map_err(Into::into).and_then(|rt|rt.block_on(execute(input))));
    match result {Ok(result)=>{println!("{}",serde_json::to_string_pretty(&result).unwrap());if result["status"]=="error" {ExitCode::from(1)} else {ExitCode::SUCCESS}},Err(e)=>super::exit_with(&e)}
}
