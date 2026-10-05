use axum::http::{HeaderMap, header};
use sha2::{Digest,Sha256};
use std::{collections::HashMap,fs::{self,OpenOptions},io::Write,path::Path,sync::Mutex,time::{Duration,Instant}};
use subtle::ConstantTimeEq;
use uuid::Uuid;

const SESSION_AGE:Duration=Duration::from_secs(7*24*60*60);

pub struct AppAuth {
    token_hash:[u8;32],
    sessions:Mutex<HashMap<[u8;32],Instant>>,
    secure_cookie:bool,
}

fn digest(text:&str)->[u8;32] {Sha256::digest(text.as_bytes()).into()}
fn random_token()->String {format!("{}{}",Uuid::new_v4().simple(),Uuid::new_v4().simple())}

fn read_existing_token(path:&Path)->Result<String,String> {
    let metadata=fs::symlink_metadata(path).map_err(|e|e.to_string())?;
    if !metadata.file_type().is_file() {return Err("data/access_token must be a regular file".into());}
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode()&0o077!=0 {return Err("data/access_token is readable by other users; set its permissions to 600".into());}
    }
    let token=fs::read_to_string(path).map_err(|e|e.to_string())?.trim().to_string();
    if token.len()<32 {return Err("Stored access token is too short".into());}
    Ok(token)
}

fn read_or_create_token(data_dir:&Path)->Result<String,String> {
    if let Ok(token)=std::env::var("SLICER_AGENT_TOKEN") {
        if token.len()<32 {return Err("SLICER_AGENT_TOKEN must contain at least 32 characters".into());}
        return Ok(token);
    }
    let path=data_dir.join("access_token");
    match fs::symlink_metadata(&path) {
        Ok(_)=>read_existing_token(&path),
        Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{
            let token=random_token();
            let mut options=OpenOptions::new();options.write(true).create_new(true);
            #[cfg(unix)] {use std::os::unix::fs::OpenOptionsExt;options.mode(0o600);}
            match options.open(&path) {
                Ok(mut file)=>{file.write_all(format!("{token}\n").as_bytes()).and_then(|_|file.sync_all()).map_err(|e|e.to_string())?;Ok(token)},
                Err(error) if error.kind()==std::io::ErrorKind::AlreadyExists=>read_existing_token(&path),
                Err(error)=>Err(error.to_string()),
            }
        }
        Err(error)=>Err(error.to_string()),
    }
}

impl AppAuth {
    pub fn new(data_dir:&Path,secure_cookie:bool)->Result<Self,String> {
        let token=read_or_create_token(data_dir)?;
        Ok(Self{token_hash:digest(&token),sessions:Mutex::new(HashMap::new()),secure_cookie})
    }

    pub fn verify_token(&self,token:&str)->bool {
        if token.len()>4096 {return false;}
        bool::from(self.token_hash.ct_eq(&digest(token)))
    }

    fn cookie_name(&self)->&'static str {
        if self.secure_cookie {"__Host-sliceagent_session"} else {"sliceagent_session"}
    }

    pub fn authenticated(&self,headers:&HeaderMap)->bool {
        if let Some(bearer)=headers.get(header::AUTHORIZATION).and_then(|v|v.to_str().ok()).and_then(|v|v.strip_prefix("Bearer ")) {
            return self.verify_token(bearer);
        }
        let cookie=headers.get(header::COOKIE).and_then(|v|v.to_str().ok()).and_then(|value|value.split(';').find_map(|pair|pair.trim().split_once('=').filter(|(name,_)|*name==self.cookie_name()).map(|(_,value)|value)));
        let Some(cookie)=cookie else {return false};
        if cookie.len()!=64||!cookie.bytes().all(|byte|byte.is_ascii_hexdigit()){return false;}
        let hash=digest(cookie);
        let sessions=self.sessions.lock().unwrap();
        sessions.get(&hash).is_some_and(|expiry|*expiry>Instant::now())
    }

    pub fn login_cookie(&self)->String {
        let session=random_token();
        let mut sessions=self.sessions.lock().unwrap();
        sessions.retain(|_,expiry|*expiry>Instant::now());
        if sessions.len()>=100 {sessions.clear();}
        sessions.insert(digest(&session),Instant::now()+SESSION_AGE);
        format!("{}={session}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",self.cookie_name(),SESSION_AGE.as_secs(),if self.secure_cookie {"; Secure"}else{""})
    }

    pub fn logout_cookie(&self,headers:&HeaderMap)->String {
        if let Some(cookie)=headers.get(header::COOKIE).and_then(|v|v.to_str().ok()).and_then(|value|value.split(';').find_map(|pair|pair.trim().split_once('=').filter(|(name,_)|*name==self.cookie_name()).map(|(_,value)|value))) {
            self.sessions.lock().unwrap().remove(&digest(cookie));
        }
        format!("{}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{}",self.cookie_name(),if self.secure_cookie {"; Secure"}else{""})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_token_and_cookie_stay_private() {
        let dir=tempfile::tempdir().unwrap();
        let auth=AppAuth::new(dir.path(),false).unwrap();
        let token=fs::read_to_string(dir.path().join("access_token")).unwrap();
        assert!(auth.verify_token(token.trim()));
        assert!(!auth.verify_token("wrong token"));
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(dir.path().join("access_token")).unwrap().permissions().mode()&0o777,0o600);
        }
        let cookie=auth.login_cookie();
        let mut headers=HeaderMap::new();
        headers.insert(header::COOKIE,cookie.split(';').next().unwrap().parse().unwrap());
        assert!(auth.authenticated(&headers));
        auth.logout_cookie(&headers);
        assert!(!auth.authenticated(&headers));
        let remote=AppAuth::new(dir.path(),true).unwrap();
        let cookie=remote.login_cookie();
        assert!(cookie.starts_with("__Host-sliceagent_session="));
        assert!(cookie.contains("; Secure"));
    }
}
