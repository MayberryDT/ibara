// ibara page reader. Runs in the extension's isolated world of the focused
// tab's top frame, injected by worker.js for one request at a time. It reads
// the page and never clicks or types: ibara does every click and keypress
// through Cua's real pointer and keyboard (docs/IMPLEMENTATION_PLAN.md,
// browser route decision, 26 September 2026).
//
// Operations (message.op):
//   observe  ranked page elements; each element is kept for 30 s under an
//            opaque capture/token so a later request can find it again
//   check    whether text is on the page
//   locate   scroll an element into view, wait until it is at rest and return
//            an uncovered point in it (CSS pixels of the viewport) with the
//            window and viewport sizes (with args.value, for a list: also the
//            label of that option), or {moving:true} if it does not come to
//            rest; arms a watch (message.watch) for the next trusted pointer
//            press and opens a port to the worker, over which the page
//            reports that press, a form it submits and the page leaving,
//            the moment each happens
//   verify   whether that press reached the element (or something inside it)
//   selected the label that list now shows
//   reveal   scroll an element into view
//   field    whether the element last located has the keyboard focus (the
//            page has it, not the address bar) and the text it holds
//   keys     whether a key pressed now (args.key: enter or space) submits a
//            form in this page
export async function documentOperation(message) {
  // The submit button of a form that posts, and a field Return in which
  // submits such a form (implicit submission: through the form's first submit
  // button while it is enabled, from a text field, a checkbox or a radio
  // button; without one, from the form's only text field). A form that gets
  // (a site search) only opens a page, so it does not count. Observe marks
  // both, keys asks of the focus.
  const submitButton = (el) => ((el instanceof HTMLButtonElement && el.type === 'submit') || (el instanceof HTMLInputElement && ['submit', 'image'].includes(el.type))) && !!el.form;
  const posts = (form, button) => ((button && button.getAttribute('formmethod')) || form.method).toLowerCase() === 'post';
  const submitter = (el) => submitButton(el) && posts(el.form, el);
  const TYPING = ['text', 'search', 'url', 'tel', 'email', 'password', 'number', 'date', 'datetime-local', 'month', 'time', 'week'];
  const enterSubmits = (el) => {
    if (!(el instanceof HTMLInputElement) || !(TYPING.includes(el.type) || el.type === 'checkbox' || el.type === 'radio') || !el.form)
      return false;
    const fields = [...el.form.elements];
    const button = fields.find(submitButton);
    if (button)
      return !button.matches(':disabled') && posts(el.form, button);
    return TYPING.includes(el.type) && posts(el.form, null) && fields.filter(f => f instanceof HTMLInputElement && TYPING.includes(f.type)).length === 1;
  };
  const collect = function collectAccessibleNodesInDocument(query, onNode) {
    const normalize = (s) => (s || '').replace(/\s+/g, ' ').trim();
    const visible = (el) => {
        if (el.closest('[hidden],[aria-hidden="true"],[inert]'))
            return false;
        const style = getComputedStyle(el);
        return style.display !== 'none' && style.visibility !== 'hidden' && style.visibility !== 'collapse' && el.getClientRects().length > 0;
    };
    // An element's text, and its card text below, once per read: every
    // element inside asks again, and a long page (a Wikipedia article, a
    // repository page) otherwise clones its whole body for each link.
    const contents = new Map();
    const content = (el) => {
        let text = contents.get(el);
        if (text === undefined) {
            const clone = el.cloneNode(true);
            clone.querySelectorAll('input,textarea,select,script,style,[hidden],[aria-hidden="true"]').forEach(n => n.remove());
            text = normalize(clone.textContent);
            contents.set(el, text);
        }
        return text;
    };
    const explicitName = (el) => {
        const refs = normalize(el.getAttribute('aria-labelledby')).split(' ').filter(Boolean);
        if (refs.length) {
            const value = refs.map(ref => document.getElementById(ref)).filter(Boolean).map(n => content(n)).join(' ');
            if (normalize(value))
                return normalize(value);
        }
        return normalize(el.getAttribute('aria-label'));
    };
    const name = (el) => {
        const explicit = explicitName(el);
        if (explicit)
            return explicit;
        // Browser row names separate cells even when HTML has no whitespace
        // between </td><td>. Raw textContent would concatenate their words.
        if (el.tagName === 'TR')
            return Array.from(el.children).map(content).filter(Boolean).join(' ');
        if (el.tagName === 'FIELDSET')
            return normalize(el.querySelector(':scope > legend')?.textContent);
        const labels = el.labels;
        if (labels?.length)
            return Array.from(labels).map(content).join(' ');
        if (el.tagName === 'IMG')
            return normalize(el.getAttribute('alt'));
        if (el.tagName === 'INPUT') {
            const input = el;
            if (['button', 'submit', 'reset'].includes(input.type))
                return input.value || { submit: 'Submit', reset: 'Reset' }[input.type] || '';
            if (input.type === 'image')
                return normalize(input.alt);
            return normalize(el.getAttribute('title'));
        }
        if (['FORM', 'SECTION', 'TEXTAREA', 'SELECT', 'UL', 'OL', 'LI', 'TABLE', 'TBODY', 'THEAD', 'TFOOT'].includes(el.tagName))
            return normalize(el.getAttribute('title'));
        const ariaRole = normalize(el.getAttribute('role')).split(' ')[0];
        const contentNames = ['button', 'cell', 'checkbox', 'columnheader', 'gridcell', 'heading', 'link', 'menuitem', 'menuitemcheckbox', 'menuitemradio', 'option', 'radio', 'row', 'rowheader', 'switch', 'tab', 'tooltip', 'treeitem'];
        if ((ariaRole && !contentNames.includes(ariaRole)) || ['DIALOG', 'NAV', 'MAIN', 'ASIDE'].includes(el.tagName))
            return normalize(el.getAttribute('title'));
        return content(el) || normalize(el.getAttribute('title'));
    };
    const role = (el) => {
        const explicit = normalize(el.getAttribute('role')).split(' ')[0];
        if (explicit && !['none', 'presentation'].includes(explicit))
            return explicit;
        switch (el.tagName) {
            case 'BUTTON': return 'button';
            case 'A': return el.hasAttribute('href') ? 'link' : '';
            case 'TEXTAREA': return 'textbox';
            case 'SELECT': return el.multiple || el.size > 1 ? 'listbox' : 'combobox';
            case 'FIELDSET': return 'group';
            case 'FORM': return name(el) ? 'form' : '';
            case 'SECTION': return name(el) ? 'region' : '';
            case 'DIALOG': return 'dialog';
            case 'NAV': return 'navigation';
            case 'MAIN': return 'main';
            case 'ASIDE': return 'complementary';
            case 'IMG': return 'img';
            case 'TR': return 'row';
            case 'LI': return 'listitem';
            case 'INPUT': {
                const type = el.type;
                if (['button', 'submit', 'reset', 'image'].includes(type))
                    return 'button';
                if (type === 'checkbox' || type === 'radio')
                    return type;
                if (type === 'range')
                    return 'slider';
                if (type === 'number')
                    return 'spinbutton';
                if (type === 'search')
                    return 'searchbox';
                // File inputs use a dedicated exact-label recipe, not getByRole.
                if (type === 'file')
                    return name(el) ? 'file_input' : '';
                if (['password', 'hidden', 'color', 'date', 'datetime-local', 'month', 'time', 'week'].includes(type))
                    return '';
                return 'textbox';
            }
            default: return /^H[1-6]$/.test(el.tagName) ? 'heading' : '';
        }
    };
    // A short element headed by one visible heading, or with a small action
    // group and text of its own: the card an element inside it belongs to.
    // '' when it is not one.
    const cards = new Map();
    const card = (p) => {
        let text = cards.get(p);
        if (text !== undefined)
            return text;
        text = content(p);
        if (text.length > 400)
            text = '';
        else {
            const headings = p.querySelectorAll('h1,h2,h3,h4,h5,h6,[role="heading"]');
            const controls = Array.from(p.querySelectorAll('a[href],button,[role="link"],[role="button"]'));
            // Many cards identify records with plain text, not a heading. Require
            // a small action group and independent text beyond its control labels.
            const remaining = controls.reduce((s, control) => s.replace(content(control), ''), text).trim();
            const plainCard = controls.length > 0 && controls.length <= 4 && remaining.length >= 3;
            if (!((headings.length === 1 && visible(headings[0])) || plainCard))
                text = '';
        }
        cards.set(p, text);
        return text;
    };
    const containers = new Set(['form', 'group', 'region', 'dialog', 'navigation', 'main', 'complementary', 'row', 'listitem']);
    const needle = normalize(query).toLowerCase();
    const elements = document.querySelectorAll('button,input,textarea,select,a[href],fieldset,form,section,dialog,nav,main,aside,img,h1,h2,h3,h4,h5,h6,[role]');
    const nodes = [];
    for (const el of elements) {
        if (!visible(el))
            continue;
        const r = role(el);
        if (!r)
            continue;
        const n = name(el);
        const ancestors = [];
        // Keep nearby card identity separately from accessibility ancestors. Generic
        // div cards have no ARIA role; inventing one would break role-based locators.
        let context = '';
        for (let p = el.parentElement; p; p = p.parentElement) {
            const pr = role(p);
            if (containers.has(pr)) {
                const pn = name(p);
                // List items do not take their accessible name from their contents.
                // Keep textual context separately for a role-scoped exact text filter.
                ancestors.unshift({ role: pr, name: pn, ...(pr === 'listitem' && !pn ? { text: content(p) } : {}) });
            }
            if (!context && ['DIV', 'ARTICLE', 'SECTION', 'LI'].includes(p.tagName))
                context = card(p);
        }
        if (needle && ![r, n, context, ...ancestors.map(a => `${a.role} ${a.text || a.name}`)].join(' ').toLowerCase().includes(needle))
            continue;
        const states = ['visible'];
        if (ancestors.map(a => `${a.role}:${a.text || a.name}`).join(' > ').length > 500)
            states.push('context_truncated');
        const disabled = el.matches(':disabled') || el.getAttribute('aria-disabled') === 'true';
        states.push(disabled ? 'disabled' : 'enabled');
        if (document.activeElement === el)
            states.push('focused');
        if (el.checked === true || el.getAttribute('aria-checked') === 'true')
            states.push('checked');
        if (submitter(el))
            states.push('submits');
        else if (enterSubmits(el))
            states.push('enter_submits');
        const actions = [];
        if (!disabled) {
            if (['button', 'link', 'checkbox', 'radio', 'menuitem', 'tab'].includes(r))
                actions.push('click');
            if (['textbox', 'searchbox', 'spinbutton'].includes(r) && !el.readOnly)
                actions.push('fill', 'press');
            if (['combobox', 'listbox'].includes(r))
                actions.push('select');
            if (r === 'file_input')
                actions.push('upload');
        }
        const value = r !== 'file_input' && ['INPUT', 'TEXTAREA', 'SELECT'].includes(el.tagName) ? String(el.value ?? '') : undefined;
        const node = { role: r, name: n, states, actions, ancestors, ...(context ? { context } : {}), ...(value !== undefined ? { value } : {}) };
        nodes.push(node);
        onNode?.(el, node);
    }
    return nodes;
};
  // The worker already requires the tab to be active in the focused window.
  // Keyboard focus may sit in the address bar; a click gives the page focus.
  const safe=()=>Date.now()<message.deadline&&document.visibilityState==='visible';
  const refuse=()=>({refused:true});
  if(!safe())return refuse();
  const args=message.args;
  const normalize=s=>(s||'').replace(/\s+/g,' ').trim();
  // This global belongs only to the extension's isolated world, not page
  // scripts. An updated extension can meet the state and press listener its
  // previous copy left in an open page, so the key changes whenever they do.
  const key='__ibaraPageReaderV4';
  const state=globalThis[key] ||= {captures:new Map(),watch:null,listening:false,selecting:null,located:null,port:null};
  if(!state.listening){
    // Which element the next trusted press reached, whether it submitted a
    // form, and whether the page then leaves. Page scripts cannot see or
    // forge this: synthetic events are not trusted. Each report goes to the
    // worker at once over the port locate opened before the press, so a
    // navigation the press starts (a link, a form's submit button) cannot
    // lose it.
    const post=m=>{try{state.port?.postMessage(m);}catch{}};
    addEventListener('pointerdown',e=>{
      const w=state.watch;
      if(!w||w.result||!e.isTrusted)return;
      w.result={hit:reaches(w.el,e.target)};
      post({press:{watch:w.id,hit:w.result.hit}});
    },true);
    addEventListener('submit',e=>{
      const w=state.watch;
      if(w&&e.isTrusted)post({submit:{watch:w.id,hit:!!e.submitter&&reaches(w.el,e.submitter)}});
    },true);
    addEventListener('beforeunload',()=>{const w=state.watch;if(w)post({leaving:{watch:w.id}});},true);
    state.listening=true;
  }
  const pack=(el,node)=>{
    const ancestor=[...node.ancestors.map(a=>`${a.role}:${a.text||a.name}`),...(node.context?[`card:${node.context}`]:[])].join(' > ');
    return {role:node.role,name:node.name.slice(0,1000),ancestor:ancestor.slice(0,500),
      states:[...node.states,...(node.name.length>1000||ancestor.length>500?['context_truncated']:[])].filter(s=>s!=='focused'),
      actions:node.actions.filter(a=>a!=='upload'),href:el instanceof HTMLAnchorElement?el.href:''};
  };
  // The element a capture/token names, only while the page still shows it
  // the same way (same URL, same element, same role, name and context). No age
  // limit: a step held for a person's approval may run minutes later.
  const find=()=>{
    const capture=state.captures.get(args.capture);
    const old=capture?.targets.get(args.token);
    if(!old||capture.url!==location.href||!old.el.isConnected)return null;
    let current;
    collect('',(el,node)=>{if(el===old.el)current=pack(el,node);});
    const same=current&&JSON.stringify({...current,states:[]})===JSON.stringify({...old.packed,states:[]});
    return same?old.el:null;
  };
  const inView=el=>{const r=el.getBoundingClientRect();return r.top>=0&&r.left>=0&&r.bottom<=innerHeight&&r.right<=innerWidth;};
  // Whether a press on `hit` acts on el: el itself or something inside it,
  // or, for a checkbox or radio button, its own label. Sites often hide the
  // native box under a styled label (Bootstrap's custom-control-input, the
  // Tailwind sr-only pattern), and a press on the label toggles it; a press
  // on a link or other control inside that label does not.
  const CONTROLS='a[href],button,input,select,textarea,summary,iframe,[contenteditable=""],[contenteditable="true"],[role=button],[role=link],[role=checkbox],[role=radio],[role=switch]';
  const toggle=el=>el instanceof HTMLInputElement&&(el.type==='checkbox'||el.type==='radio');
  const reaches=(el,hit)=>{
    if(!hit)return false;
    if(hit===el||el.contains(hit))return true;
    const label=toggle(el)?[...(el.labels||[])].find(l=>l.contains(hit)):null;
    if(!label)return false;
    for(let n=hit;n!==label;n=n.parentElement)if(n.matches(CONTROLS))return false;
    return true;
  };
  // A point that reaches el, inside el or else inside one of its labels.
  const reachable=el=>{
    for(const box of [el,...(toggle(el)?el.labels||[]:[])]){
      const r=[...box.getClientRects()].find(q=>q.width>0&&q.height>0)||box.getBoundingClientRect();
      const left=Math.max(0,r.left),top=Math.max(0,r.top),right=Math.min(innerWidth,r.right),bottom=Math.min(innerHeight,r.bottom);
      if(right-left<1||bottom-top<1)continue;
      for(const [fx,fy] of [[.5,.5],[.3,.5],[.7,.5],[.5,.3],[.5,.7],[.2,.2],[.8,.8]]){
        const x=left+(right-left)*fx,y=top+(bottom-top)*fy;
        if(reaches(el,document.elementFromPoint(x,y)))return {x:Math.round(x*100)/100,y:Math.round(y*100)/100};
      }
    }
    return null;
  };
  // The next rendered frame; after 100 ms without one, go on.
  const frame=()=>new Promise(done=>{requestAnimationFrame(()=>done());setTimeout(done,100);});
  const centre=el=>{const r=el.getBoundingClientRect();return [r.left+r.width/2,r.top+r.height/2];};
  if(message.op==='keys') {
    // Not when the keyboard is elsewhere (the address bar). A form inside a
    // frame is not seen: the focus is then the frame itself.
    const el=document.hasFocus()?document.activeElement:null;
    return {page:!!el,submits:!!el&&(submitter(el)||(args.key==='enter'&&enterSubmits(el)))};
  }
  if(message.op==='check') {
    // innerText excludes hidden rendered text and does not expose password values.
    const text=(document.body?.innerText||'').slice(0,200000);
    return {found:normalize(text).includes(normalize(args.text)),truncated:(document.body?.innerText||'').length>200000};
  }
  if(message.op==='verify') {
    const w=state.watch;state.watch=null;
    return {observed:!!w?.result,hit:w?.result?w.result.hit:null};
  }
  if(message.op==='selected') {
    const el=state.selecting;
    if(!el||!el.isConnected)return refuse();
    return {text:normalize(el.selectedOptions[0]?.text),value:el.value};
  }
  if(message.op==='field') {
    // The element the last locate pointed at (the field ibara just clicked).
    const located=state.located;
    const el=located&&located.capture===args.capture&&located.token===args.token?located.el:null;
    if(!el||!el.isConnected)return refuse();
    const text=el instanceof HTMLInputElement||el instanceof HTMLTextAreaElement;
    const active=document.activeElement;
    // document.hasFocus() is false while the address bar or another window has the keyboard.
    const focused=document.hasFocus()&&(text?active===el:el.isContentEditable&&(active===el||el.contains(active)));
    if(typeof args.expected!=='string')return {focused};
    // Only whether the field holds what ibara typed: its value (a password,
    // a large text) never leaves the page. Text areas turn line ends into \n.
    const value=text?el.value:el.isContentEditable?el.innerText.replace(/\u00a0/g,' ').replace(/\n$/,''):'';
    const lines=s=>s.replace(/\r\n?/g,'\n');
    return {focused,matches:value===args.expected||lines(value)===lines(args.expected)};
  }
  if(message.op==='locate'||message.op==='reveal') {
    const el=find();
    if(!el)return refuse();
    const scrolled=!inView(el);
    if(scrolled)el.scrollIntoView({block:'center',inline:'nearest',behavior:'instant'});
    if(message.op==='reveal')return {revealed:true};
    // A list choice: the option's label, typed into the list once it is open.
    let label;
    if(args.value!==undefined) {
      if(!(el instanceof HTMLSelectElement)||el.multiple||el.size>1)return refuse();
      const wanted=normalize(args.value);
      const options=[...el.options].filter(o=>!o.disabled);
      const option=options.find(o=>o.value===args.value)||options.find(o=>normalize(o.text)===wanted);
      const texts=options.slice(0,20).map(o=>normalize(o.text));
      if(!option)return {label:'',options:texts};
      label=normalize(option.text);
      // Typing a label picks the first option that starts with it.
      const first=options.find(o=>normalize(o.text).toLowerCase().startsWith(label.toLowerCase()));
      if(first!==option)return {label:'',options:texts,reason:'another option starts with the same text'};
      state.selecting=el;
    }
    // The page answers a scroll on later frames (a header that comes back,
    // a lazy image that pushes content down), and Cua then takes 0.35–1.4 s
    // to point. So the point is chosen once the element is at rest: two
    // frames after a scroll, then only when a frame later its centre has not
    // moved and the point still reaches it. Still moving after 1 s: refused.
    if(scrolled){await frame();await frame();}
    const until=Date.now()+1000;
    let point=null,rest=false;
    while(!rest&&Date.now()<until){
      const before=centre(el);
      point=reachable(el);
      await frame();
      if(!safe()||!el.isConnected)return refuse();
      const after=centre(el);
      rest=Math.abs(after[0]-before[0])<=1&&Math.abs(after[1]-before[1])<=1&&(!point||reaches(el,document.elementFromPoint(point.x,point.y)));
    }
    if(!rest)return {moving:true};
    if(!point)return {covered:true};
    state.watch={el,id:message.watch,result:null};
    state.located={el,capture:args.capture,token:args.token};
    try{state.port?.disconnect();}catch{}
    state.port=chrome.runtime.connect({name:'press:'+message.watch});
    return {...point,watch:state.watch.id,outer:[outerWidth,outerHeight],inner:[innerWidth,innerHeight],...(label!==undefined?{label}:{})};
  }
  const page=(capture,snapshot,offset)=>{
    const nodes=snapshot.nodes.slice(offset,offset+args.limit);
    const allText=args.view==='text'?(document.body?.innerText||''):'';
    const next=offset+nodes.length<snapshot.nodes.length?offset+nodes.length:null;
    return {capture,url:location.href,title:document.title,nodes,count:snapshot.nodes.length,text:allText.slice(0,args.maxChars),
      truncated:next!==null||allText.length>args.maxChars,next_offset:next};
  };
  if(args.capture){
    const snapshot=state.captures.get(args.capture);
    if(!snapshot||snapshot.url!==location.href||Date.now()-snapshot.at>30000||snapshot.query!==(args.query||'')||
      !Number.isSafeInteger(args.offset)||args.offset<0||args.offset>=snapshot.nodes.length)return refuse();
    return page(args.capture,snapshot,args.offset);
  }
  const targets=new Map();const nodes=[];const equivalentLinks=new Set();
  collect(args.query||'',(el,node)=>{
    const packed=pack(el,node);
    // Identical navigation aliases carry no additional choice. Different hrefs
    // with indistinguishable labels/context remain ambiguous and cannot act.
    const alias=JSON.stringify([packed.role,packed.name,packed.ancestor,packed.href]);
    if(packed.role==='link'){
      if(equivalentLinks.has(alias))return;
      equivalentLinks.add(alias);
    }
    const token=crypto.randomUUID();
    targets.set(token,{el,packed});
    nodes.push({...packed,token});
  });
  const counts=new Map();
  for(const n of nodes){const k=JSON.stringify([n.role,n.name,n.ancestor]);counts.set(k,(counts.get(k)||0)+1);}
  for(const n of nodes)if(counts.get(JSON.stringify([n.role,n.name,n.ancestor]))>1)n.states.push('ambiguous');
  const capture=crypto.randomUUID();
  const snapshot={url:location.href,at:Date.now(),targets,nodes,query:args.query||''};
  state.captures.set(capture,snapshot);
  while(state.captures.size>4)state.captures.delete(state.captures.keys().next().value);
  return page(capture,snapshot,0);
}
