//! Element description: what a click on the preview becomes (design.md §8.4).

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::browser::Browser;
use crate::sanitize::clean;
use crate::{BrowserError, ClipRect, Resolved};

/// The only computed styles read.
pub(crate) const STYLE_PROPS: [&str; 9] = [
    "color",
    "background-color",
    "font-size",
    "font-weight",
    "padding",
    "margin",
    "border-radius",
    "gap",
    "display",
];

/// Moves from the element under the point: positive = parent chain, negative = first-child chain.
pub(crate) const WALK_JS: &str = r#"function(d){let e=this;if(e.nodeType!==1)e=e.parentElement||e;if(d>0){for(let i=0;i<d&&e.parentElement;i++)e=e.parentElement;}else if(d<0){for(let i=0;i<-d&&e.firstElementChild;i++)e=e.firstElementChild;}return e;}"#;

/// Fixed in-crate function run on the element: selector, role, accessible name, component,
/// pathname and computed styles. Never reads input values.
pub(crate) const INFO_JS: &str = r#"function(){
const el=this;const out={path:String(location.pathname||'/'),tag:String(el.tagName||'').toLowerCase()};
try{
const esc=function(s){return(window.CSS&&CSS.escape)?CSS.escape(s):String(s).replace(/[^a-zA-Z0-9_-]/g,'\\$&');};
const part=function(e){
 const t=e.getAttribute&&e.getAttribute('data-testid');
 if(t)return{s:'[data-testid="'+String(t).replace(/["\\]/g,'\\$&')+'"]',stop:true};
 if(e.id)return{s:e.tagName.toLowerCase()+'#'+esc(e.id),stop:true};
 let s=e.tagName.toLowerCase();
 const cls=(e.classList?Array.from(e.classList):[]).slice(0,2);
 for(const c of cls)s+='.'+esc(c);
 const up=e.parentElement;
 if(up){const same=Array.from(up.children).filter(function(c){return c.tagName===e.tagName;});if(same.length>1){
  let told=false;
  for(const a of ['aria-label','name','href','title','alt']){
   const v=e.getAttribute(a);
   if(!v||v.length>40||/[?#"\\\n]/.test(v))continue;
   if(same.filter(function(c){return c.getAttribute(a)===v;}).length===1){s+='['+a+'="'+v+'"]';told=true;break;}
  }
  if(!told)s+=':nth-of-type('+(same.indexOf(e)+1)+')';}}
 return{s:s,stop:false};};
 const parts=[];let e=el;
 for(let i=0;e&&e.nodeType===1&&i<5;i++){const p=part(e);parts.unshift(p.s);if(p.stop||e===document.body)break;e=e.parentElement;}
 out.selector=parts.join(' > ');
}catch(x){}
try{
 const tag=out.tag;const ty=String(el.getAttribute('type')||'text').toLowerCase();
 const roles={a:el.hasAttribute('href')?'link':'generic',button:'button',select:'combobox',textarea:'textbox',img:'img',nav:'navigation',main:'main',header:'banner',footer:'contentinfo',form:'form',table:'table',ul:'list',ol:'list',li:'listitem',h1:'heading',h2:'heading',h3:'heading',h4:'heading',h5:'heading',h6:'heading',dialog:'dialog',summary:'button',label:'label',p:'paragraph',section:'region',article:'article',aside:'complementary'};
 let role=el.getAttribute('role')||roles[tag]||tag;
 if(tag==='input'){role={button:'button',submit:'button',reset:'button',checkbox:'checkbox',radio:'radio',range:'slider',search:'searchbox',number:'spinbutton'}[ty]||'textbox';}
 out.role=role;
 let name=el.getAttribute('aria-label')||'';
 if(!name){const lb=el.getAttribute('aria-labelledby');if(lb){name=lb.split(/\s+/).map(function(i){const n=document.getElementById(i);return n?n.textContent:'';}).join(' ');}}
 if(!name&&tag==='img')name=el.getAttribute('alt')||'';
 if(!name&&(tag==='input'||tag==='textarea'||tag==='select')){
  if(el.labels&&el.labels.length)name=el.labels[0].textContent||'';
  if(!name)name=el.getAttribute('placeholder')||el.getAttribute('name')||'';
 } else if(!name){name=el.textContent||'';}
 if(!name)name=el.getAttribute('title')||'';
 out.name=String(name).replace(/\s+/g,' ').trim().slice(0,400);
}catch(x){}
try{
 let comp=null;
 const fk=Object.keys(el).find(function(k){return k.indexOf('__reactFiber$')===0||k.indexOf('__reactInternalInstance$')===0;});
 if(fk){let f=el[fk];while(f){const t=f.type;if(t&&typeof t!=='string'){const n=t.displayName||t.name||(t.render&&(t.render.displayName||t.render.name));if(n){comp=[String(n),'react'];break;}}f=f.return;}}
 if(!comp&&el.__vueParentComponent){const t=(el.__vueParentComponent.type)||{};const n=t.__name||t.name||(t.__file?String(t.__file).split('/').pop().replace(/\.vue$/,''):'');if(n)comp=[String(n),'vue'];}
 if(!comp&&el.__svelte_meta&&el.__svelte_meta.loc&&el.__svelte_meta.loc.file){comp=[String(el.__svelte_meta.loc.file).split('/').pop().replace(/\.svelte$/,''),'svelte'];}
 out.comp=comp;
}catch(x){}
try{const cs=getComputedStyle(el);out.styles={};for(const p of ['color','background-color','font-size','font-weight','padding','margin','border-radius','gap','display'])out.styles[p]=cs.getPropertyValue(p);}catch(x){}
return out;}"#;

fn str_of(v: &Value) -> Option<String> {
    v.as_str().map(clean).filter(|s| !s.is_empty())
}

pub(crate) fn quad_rect(q: &Value) -> Option<ClipRect> {
    let n: Vec<f64> = q.as_array()?.iter().filter_map(Value::as_f64).collect();
    if n.len() < 8 {
        return None;
    }
    let xs = n.iter().step_by(2);
    let ys = n.iter().skip(1).step_by(2);
    let (x0, x1) = xs.fold((f64::MAX, f64::MIN), |(a, b), v| (a.min(*v), b.max(*v)));
    let (y0, y1) = ys.fold((f64::MAX, f64::MIN), |(a, b), v| (a.min(*v), b.max(*v)));
    Some(ClipRect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    })
}

/// Build the [`Resolved`] from the in-page result, the box and (preferred) CSS styles.
pub(crate) fn assemble(
    info: &Value,
    rect: ClipRect,
    css: Option<BTreeMap<String, String>>,
) -> Resolved {
    let path = str_of(&info["path"]).unwrap_or_else(|| "/".into());
    let selector = str_of(&info["selector"]);
    let tag = str_of(&info["tag"]).unwrap_or_default();
    let role = str_of(&info["role"]).unwrap_or_else(|| tag.clone());
    let name = str_of(&info["name"]).map(|n| n.replace('"', "'"));
    let role_name = match (role.is_empty(), name) {
        (true, _) => None,
        (false, Some(n)) => Some(clean(&format!("{role} \"{n}\""))),
        (false, None) => Some(role),
    };
    let component = info["comp"]
        .as_array()
        .and_then(|a| Some((str_of(a.first()?)?, str_of(a.get(1)?)?)));
    let mut styles = BTreeMap::new();
    let page: BTreeMap<String, String> = info["styles"]
        .as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    for p in STYLE_PROPS {
        let v = css.as_ref().and_then(|m| m.get(p)).or_else(|| page.get(p));
        if let Some(v) = v {
            styles.insert(p.to_string(), clean(v));
        }
    }
    let mut label = role_name
        .clone()
        .or_else(|| selector.clone())
        .unwrap_or(tag);
    if let Some((c, _)) = &component {
        label = clean(&format!("{label} - {c}"));
    }
    Resolved {
        path,
        selector,
        role_name,
        component,
        x: rect.x,
        y: rect.y,
        w: rect.w,
        h: rect.h,
        styles,
        label,
    }
}

impl Browser {
    /// Describe the element at `(x, y)`; `depth` > 0 walks to a parent, < 0 down the first-child chain.
    pub async fn resolve(&self, x: f64, y: f64, depth: i32) -> Result<Resolved, BrowserError> {
        self.sh.disclose(self.read_element(x, y, depth)).await
    }

    async fn read_element(&self, x: f64, y: f64, depth: i32) -> Result<Resolved, BrowserError> {
        let sh = &self.sh;
        // Chrome hit-tests page coordinates; the point is on the frame, which scrolls.
        let (x, y) = sh.layout().await?.page_point(x, y);
        let n = sh
            .call(
                "DOM.getNodeForLocation",
                json!({"x": x.round() as i64, "y": y.round() as i64}),
            )
            .await?;
        let backend = n["backendNodeId"]
            .as_i64()
            .ok_or_else(|| BrowserError::Protocol("no node at that point".into()))?;
        let r = sh
            .call(
                "DOM.resolveNode",
                json!({"backendNodeId": backend, "objectGroup": "archon-design"}),
            )
            .await?;
        let obj = r["object"]["objectId"]
            .as_str()
            .ok_or_else(|| BrowserError::Protocol("node has no object".into()))?
            .to_string();
        let out = self.resolve_object(&obj, depth).await;
        let _ = sh
            .call(
                "Runtime.releaseObjectGroup",
                json!({"objectGroup": "archon-design"}),
            )
            .await;
        out
    }

    async fn resolve_object(&self, obj: &str, depth: i32) -> Result<Resolved, BrowserError> {
        let sh = &self.sh;
        let w = sh
            .call(
                "Runtime.callFunctionOn",
                json!({"objectId": obj, "functionDeclaration": WALK_JS, "arguments": [{"value": depth}], "objectGroup": "archon-design"}),
            )
            .await?;
        let el = w["result"]["objectId"]
            .as_str()
            .ok_or_else(|| BrowserError::Protocol("walk gave no element".into()))?
            .to_string();
        let bm = sh.call("DOM.getBoxModel", json!({"objectId": el})).await?;
        let rect = quad_rect(&bm["model"]["border"])
            .ok_or_else(|| BrowserError::Protocol("no box model".into()))?;
        let _ = sh.call("DOM.describeNode", json!({"objectId": el})).await?;
        let info = sh
            .call(
                "Runtime.callFunctionOn",
                json!({"objectId": el, "functionDeclaration": INFO_JS, "returnByValue": true}),
            )
            .await?;
        let css = self.computed_styles(&el).await;
        Ok(assemble(&info["result"]["value"], rect, css))
    }

    async fn computed_styles(&self, obj: &str) -> Option<BTreeMap<String, String>> {
        let sh = &self.sh;
        sh.call("DOM.getDocument", json!({"depth": 0})).await.ok()?;
        let nid = sh
            .call("DOM.requestNode", json!({"objectId": obj}))
            .await
            .ok()?["nodeId"]
            .as_i64()?;
        let r = sh
            .call("CSS.getComputedStyleForNode", json!({"nodeId": nid}))
            .await
            .ok()?;
        let m: BTreeMap<String, String> = r["computedStyle"]
            .as_array()?
            .iter()
            .filter_map(|p| {
                Some((
                    p["name"].as_str()?.to_string(),
                    p["value"].as_str()?.to_string(),
                ))
            })
            .filter(|(k, _)| STYLE_PROPS.contains(&k.as_str()))
            .collect();
        (!m.is_empty()).then_some(m)
    }

    /// Box of the element `selector` matches, in CSS px; `None` when nothing
    /// matches. A selector may match several elements (a nav's links share
    /// one); then the match nearest `near`, the box recorded with it, wins,
    /// and without `near` the first.
    pub async fn resolve_selector(
        &self,
        selector: &str,
        near: Option<ClipRect>,
    ) -> Result<Option<ClipRect>, BrowserError> {
        self.sh.disclose(self.read_selector(selector, near)).await
    }

    pub(crate) async fn read_selector(
        &self,
        selector: &str,
        near: Option<ClipRect>,
    ) -> Result<Option<ClipRect>, BrowserError> {
        let sh = &self.sh;
        let doc = sh.call("DOM.getDocument", json!({"depth": 0})).await?;
        let root = doc["root"]["nodeId"].as_i64().unwrap_or(1);
        let q = match sh
            .call(
                "DOM.querySelectorAll",
                json!({"nodeId": root, "selector": selector}),
            )
            .await
        {
            Ok(q) => q,
            Err(BrowserError::Protocol(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        let ids: Vec<i64> = q["nodeIds"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_i64)
                    .filter(|n| *n != 0)
                    .collect()
            })
            .unwrap_or_default();
        let wanted = if near.is_some() { MAX_MATCHES } else { 1 };
        let mut boxes = Vec::new();
        for nid in ids.into_iter().take(wanted) {
            match sh.call("DOM.getBoxModel", json!({"nodeId": nid})).await {
                Ok(bm) => boxes.extend(quad_rect(&bm["model"]["border"])),
                // An element with no box (display: none) is not the one meant.
                Err(BrowserError::Protocol(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(nearest(&boxes, near))
    }
}

/// The most matches of one selector measured to find the one meant.
const MAX_MATCHES: usize = 64;

/// The box among `boxes` nearest `near` (centres first, then size), else the
/// first.
pub(crate) fn nearest(boxes: &[ClipRect], near: Option<ClipRect>) -> Option<ClipRect> {
    let Some(near) = near else {
        return boxes.first().copied();
    };
    let centre = |r: &ClipRect| (r.x + r.w / 2.0, r.y + r.h / 2.0);
    let (nx, ny) = centre(&near);
    let distance = |r: &ClipRect| {
        let (x, y) = centre(r);
        (x - nx).hypot(y - ny) + (r.w - near.w).abs() + (r.h - near.h).abs()
    };
    boxes
        .iter()
        .copied()
        .min_by(|a, b| distance(a).total_cmp(&distance(b)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> ClipRect {
        ClipRect { x, y, w, h }
    }

    /// LOW-crop: `nav > a` matches every link; the final picture of the
    /// "New task" link must not be taken of the first one, "Tasks".
    #[test]
    fn a_selector_matching_several_takes_the_one_recorded() {
        let tasks = rect(230.0, 21.0, 40.0, 22.0);
        let new_task = rect(297.0, 21.0, 69.0, 22.0);
        let both = [tasks, new_task];
        assert_eq!(nearest(&both, Some(new_task)), Some(new_task));
        // The page moved a little since the note: still the nearest.
        assert_eq!(
            nearest(&both, Some(rect(290.0, 25.0, 72.0, 22.0))),
            Some(new_task)
        );
        assert_eq!(nearest(&both, None), Some(tasks), "no record: the first");
        assert_eq!(nearest(&[], Some(new_task)), None);
    }

    #[test]
    fn selectors_tell_same_tag_siblings_apart() {
        assert!(INFO_JS.contains(":nth-of-type("));
        assert!(
            INFO_JS.contains("same.length>1"),
            "only when a sibling shares the tag"
        );
        // A stable attribute first, so a sibling inserted before the element
        // does not move its selector; never a form field's value.
        assert!(INFO_JS.contains("['aria-label','name','href','title','alt']"));
        assert!(!INFO_JS.contains("'value'"));
    }

    // T-U8
    #[test]
    fn sanitising_and_label() {
        let info = json!({
            "path": "/set\u{7}tings",
            "tag": "button",
            "selector": format!("form#settings > button.{}", "x".repeat(300)),
            "role": "button",
            "name": "Save\u{0} \"changes\"\n",
            "comp": ["Save\u{1b}Button", "react"],
            "styles": {"color": "rgb(1, 2, 3)", "evil": "x", "display": "blo\u{0}ck"},
        });
        let r = assemble(
            &info,
            ClipRect {
                x: 1.0,
                y: 2.0,
                w: 3.0,
                h: 4.0,
            },
            None,
        );
        assert_eq!(r.path, "/settings");
        assert_eq!(r.selector.as_ref().unwrap().chars().count(), 200);
        assert_eq!(r.role_name.as_deref(), Some("button \"Save 'changes'\""));
        assert_eq!(r.component, Some(("SaveButton".into(), "react".into())));
        assert_eq!(r.label, "button \"Save 'changes'\" - SaveButton");
        assert_eq!(r.styles.len(), 2);
        assert_eq!(r.styles["display"], "block");
        assert!(r.label.chars().count() <= 400);
    }

    #[test]
    fn css_styles_preferred_and_quad() {
        let css: BTreeMap<_, _> = [("color".to_string(), "red".to_string())].into();
        let r = assemble(
            &json!({"tag":"div","styles":{"color":"blue","display":"flex"}}),
            ClipRect {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
            },
            Some(css),
        );
        assert_eq!(r.styles["color"], "red");
        assert_eq!(r.label, "div");
        let q = quad_rect(&json!([10, 20, 110, 20, 110, 60, 10, 60])).unwrap();
        assert_eq!((q.x, q.y, q.w, q.h), (10.0, 20.0, 100.0, 40.0));
    }

    #[test]
    fn js_never_reads_input_values() {
        assert!(!INFO_JS.contains(".value"));
    }
}
