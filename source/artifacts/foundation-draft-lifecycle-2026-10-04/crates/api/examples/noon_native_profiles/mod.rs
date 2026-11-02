//! Complete immutable pristine catalogs; never learn an expectation from the
//! application DB, its object names, its first plan or a caller-supplied profile.
use super::*;

const APPROVED: &[(&str,&str,&str)]=&[
 ("windows-pg16.15-template0",
  "1bfeae859aa78b0c7d7906c25a6e21a1765b6ee14375ee4e13602f142bb5e683",
  include_str!("../noon_pg16_native.json")),
 ("ubuntu24.04-pg16.15-0ubuntu0.24.04.1-amd64-en_US.UTF-8-template0",
  "6dc4ecef55da640b68d2d66a650189265d2dc9bb182be3c31c54bdd9823a38d9",
  include_str!("../noon_pg16_ubuntu_24_04_16_15.json")),
];

pub(super) fn native_expected(actual:&Value)->Result<Value,Error>{
 for (_,hash,raw) in APPROVED {
  require(format!("{:x}",Sha256::digest(raw.as_bytes()))==*hash)?;
  let expected:Value=serde_json::from_str(raw)?;
  if actual==&expected{return Ok(expected)}
 }
 Err("unapproved pristine native catalog".into())
}

#[cfg(test)]
mod tests {
 use super::*;
 #[test]
 fn both_pristine_catalogs_and_all_identity_digest_mutations(){
  for (_,_,raw) in APPROVED {
   let actual:Value=serde_json::from_str(raw).unwrap();
   assert_eq!(native_expected(&actual).unwrap(),actual);
   for field in ["server_version_num","functions_sha256","namespaces","relations"] {
    let mut missing=actual.clone();missing.as_object_mut().unwrap().remove(field);assert!(native_expected(&missing).is_err());
   }
   let mut extra=actual.clone();extra["untrusted_extra"]=json!(true);assert!(native_expected(&extra).is_err());
   let mut version=actual.clone();version["server_version_num"]=json!("160016");assert!(native_expected(&version).is_err());
   for index in 0..actual["namespaces"].as_array().unwrap().len(){
    for field in ["oid","owner","acl","name"] {
     let mut changed=actual.clone();changed["namespaces"][index][field]=json!("not-the-approved-identity");assert!(native_expected(&changed).is_err());
    }
   }
   for index in 0..actual["relations"].as_array().unwrap().len(){
    for field in ["oid","namespace","name","sha256"] {
     let mut changed=actual.clone();changed["relations"][index][field]=json!("not-the-approved-identity");assert!(native_expected(&changed).is_err());
    }
   }
   let mut functions=actual.clone();functions["functions_sha256"]=json!("0".repeat(64));assert!(native_expected(&functions).is_err());
   let mut subset=actual.clone();subset["relations"].as_array_mut().unwrap().pop();assert!(native_expected(&subset).is_err());
  }
  // Known platform pieces cannot be mixed into a third accepted profile.
  let mut mixed:Value=serde_json::from_str(APPROVED[0].2).unwrap();
  mixed["functions_sha256"]=serde_json::from_str::<Value>(APPROVED[1].2).unwrap()["functions_sha256"].clone();
  assert!(native_expected(&mixed).is_err());
 }
}
