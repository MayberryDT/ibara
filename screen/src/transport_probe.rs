//! Private loopback receiver for the real sender; never built into releases.
use anyhow::{Context,Result,ensure};
use serde_json::json;
use tokio::io::{AsyncBufReadExt,AsyncReadExt,BufReader};
use ibara_screen::wire::{self,VideoHeader};
use std::{path::Path,time::Duration};
pub async fn run(dir:&Path,output:&Path)->Result<()> {
    let id=crate::identity::Identity::load(dir)?;
    println!("{}",json!({"t":"identity","cert_sha256":crate::identity::hash(&id.cert)}));
    let mut lines=BufReader::new(tokio::io::stdin()).lines();
    let line=tokio::time::timeout(Duration::from_secs(10),lines.next_line()).await??.context("Missing transport request")?;
    let request:serde_json::Value=serde_json::from_str(&line)?;
    let port=request["port"].as_u64().context("Missing port")?;
    ensure!(port>0 && port<=65535,"Invalid port");
    let mut endpoint=quinn::Endpoint::client("127.0.0.1:0".parse()?)?;
    endpoint.set_default_client_config(crate::identity::client(&id,request["pin"].as_str().context("Missing pin")?)?);
    let conn=tokio::time::timeout(Duration::from_secs(5),endpoint.connect(format!("127.0.0.1:{port}").parse()?,"ibara-screen")?).await??;
    let (mut send,mut recv)=conn.open_bi().await?;
    wire::write_json(&mut send,&json!({"t":"ticket","ticket":request["ticket"]})).await?;
    if request["refused"]==true {
        tokio::time::timeout(Duration::from_secs(5),conn.closed()).await?;
        println!("{}",json!({"t":"result","refused":true}));return Ok(());
    }
    let hello=tokio::time::timeout(Duration::from_secs(5),wire::read_json(&mut recv)).await??;
    ensure!(hello["t"]=="hello" && hello["input"]==false,"Expected passive watch admission");
    let mut input=conn.open_uni().await?;
    wire::write_json(&mut input,&json!({"t":"input","from":0,"events":[{"key":[30,true]},{"key":[30,false]}]})).await?;
    // Drain notices until the sequence acknowledgement, proving the attempted
    // watch input reached the real admission path without granting a turn.
    tokio::time::timeout(Duration::from_secs(5),async {
        loop {let notice=wire::read_json(&mut recv).await?;if notice["t"]=="input_ack" {break;}}
        Ok::<(),anyhow::Error>(())
    }).await??;
    let mut video=tokio::time::timeout(Duration::from_secs(5),conn.accept_uni()).await??;
    let mut header=[0;24];video.read_exact(&mut header).await?;
    let header=VideoHeader::decode(&header)?;
    let length=video.read_u32().await? as usize;
    ensure!(length>0 && length<=wire::MAX_VIDEO,"Invalid video length");
    let mut bytes=vec![0;length];video.read_exact(&mut bytes).await?;
    ensure!(header.keyframe,"First watch frame was not a keyframe");
    ensure!(request["after_sequence"].as_u64().is_none_or(|last|header.sequence>last),"Resume reused a pre-idle frame");
    std::fs::write(output,&bytes)?;
    conn.close(0u32.into(),b"proof complete");endpoint.wait_idle().await;
    println!("{}",json!({"t":"result","watch_input_acknowledged":true,"input":false,"keyframe":true,"bytes":length,
        "sequence":header.sequence,"width":hello["width"],"height":hello["height"]}));
    Ok(())
}
