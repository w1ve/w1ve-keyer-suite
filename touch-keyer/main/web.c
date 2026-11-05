// Configuration web UI and the sidetone edge stream.
//
// The panel is a name plate and two paddles; everything adjustable lives here. The page polls
// /api/status, posts speed and mode, and opens a WebSocket for keying edges, which it turns
// into tones with WebAudio. The browser is the only place audio happens — there is no hardware
// sidetone — so losing the socket costs silence and nothing else.
//
// Requests are form-encoded (wpm=20) rather than JSON: the firmware needs no JSON parser for
// two integers, and the page needs no request serialiser.
#include "web.h"

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "esp_http_server.h"
#include "esp_log.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "keyer.h"
#include "lwip/sockets.h"
#include "board.h"
#include "macro.h"
#include "net.h"
#include "remote.h"
#include "wan.h"

static const char *TAG = "web";

static const char *wan_state_name(wan_state_t st)
{
    switch (st) {
    case WAN_DISABLED: return "off";
    case WAN_WIFI_DOWN: return "no network";
    case WAN_RESOLVING: return "resolving";
    case WAN_RENDEZVOUS: return "pairing";
    case WAN_PAIRING: return "handshaking";
    case WAN_DIRECT: return "direct";
    case WAN_RELAY: return "relay";
    case WAN_RECONNECTING: return "reconnecting";
    case WAN_ERROR: return "error";
    default: return "?";
    }
}

// ------------------------------------------------------------------ sidetone edges

#define EDGE_RING 64
#define WS_CLIENTS 4

static portMUX_TYPE s_edge_lock = portMUX_INITIALIZER_UNLOCKED;
static portMUX_TYPE s_client_lock = portMUX_INITIALIZER_UNLOCKED;

static struct {
    uint32_t ms;
    bool down;
} s_edges[EDGE_RING];
static volatile uint32_t s_head, s_tail;

// Runs in the keyer task on every KEY transition: copy and go.
static void edge_push(bool down, uint32_t ms)
{
    portENTER_CRITICAL(&s_edge_lock);
    const uint32_t next = (s_head + 1) % EDGE_RING;
    if (next != s_tail) {
        s_edges[s_head].ms = ms;
        s_edges[s_head].down = down;
        s_head = next;
    }
    portEXIT_CRITICAL(&s_edge_lock);
}

static bool edge_pop(uint32_t *ms, bool *down)
{
    bool got = false;
    portENTER_CRITICAL(&s_edge_lock);
    if (s_tail != s_head) {
        *ms = s_edges[s_tail].ms;
        *down = s_edges[s_tail].down;
        s_tail = (s_tail + 1) % EDGE_RING;
        got = true;
    }
    portEXIT_CRITICAL(&s_edge_lock);
    return got;
}

// ------------------------------------------------------------------ websocket clients

static httpd_handle_t s_server;
static int s_clients[WS_CLIENTS] = {-1, -1, -1, -1};
static int64_t s_ready_us[WS_CLIENTS];
static uint8_t s_ws_fail[WS_CLIENTS];

static void ws_drop_client(int fd)
{
    portENTER_CRITICAL(&s_client_lock);
    for (int i = 0; i < WS_CLIENTS; i++) {
        if (s_clients[i] == fd) {
            s_clients[i] = -1;
            s_ready_us[i] = 0;
            s_ws_fail[i] = 0;
        }
    }
    portEXIT_CRITICAL(&s_client_lock);
}

// A slot can outlive its socket — a page reload, a phone sleeping, a board reset. If every slot
// held a dead fd, a freshly opened page would never be subscribed and the sidetone would stay
// silent with a perfectly healthy looking socket. So when all four are taken, take a dead one.
static void ws_add_client(int fd)
{
    int slot = -1;
    int snapshot[WS_CLIENTS];

    // One socket, one slot. The handshake path can reach here more than once for the same fd, and
    // a duplicate slot made the player write every edge frame to that socket twice; the page then
    // heard a 1 ms click after every element, which is what "broken up" sounded like.
    portENTER_CRITICAL(&s_client_lock);
    for (int i = 0; i < WS_CLIENTS; i++) {
        if (s_clients[i] == fd) {
            portEXIT_CRITICAL(&s_client_lock);
            ESP_LOGI(TAG, "sidetone client %d already in slot %d", fd, i);
            return;
        }
    }
    for (int i = 0; i < WS_CLIENTS; i++) {
        if (s_clients[i] < 0) {
            slot = i;
            break;
        }
    }
    for (int i = 0; i < WS_CLIENTS; i++) {
        snapshot[i] = s_clients[i];
    }
    portEXIT_CRITICAL(&s_client_lock);

    if (slot < 0) {
        // Ask the server which fds are still websockets, outside our own lock.
        for (int i = 0; i < WS_CLIENTS; i++) {
            if (httpd_ws_get_fd_info(s_server, snapshot[i]) != HTTPD_WS_CLIENT_WEBSOCKET) {
                slot = i;
                break;
            }
        }
    }
    if (slot < 0) {
        ESP_LOGW(TAG, "all %d sidetone slots are live; not subscribing %d", WS_CLIENTS, fd);
        return;
    }

    portENTER_CRITICAL(&s_client_lock);
    s_clients[slot] = fd;
    // Subscribed on the handshake request, before the server has written the 101 response.
    s_ready_us[slot] = esp_timer_get_time() + 100000;
    s_ws_fail[slot] = 0;
    portEXIT_CRITICAL(&s_client_lock);

    ESP_LOGI(TAG, "sidetone client %d subscribed in slot %d", fd, slot);
}

// Frames go straight to the socket with MSG_DONTWAIT instead of through
// httpd_ws_send_frame_async. The server's async send waits for a peer that has stopped
// draining, and it does so holding the server's lock: one backgrounded browser tab was enough
// to leave port 80 refusing connections for good while the keyer and the LAN link carried on.
// A non-blocking write either lands or it does not, and a client that will not drain is dropped.
#define WS_STALL_LIMIT 40

static bool ws_send_text(int fd, const char *payload, int len)
{
    uint8_t frame[8 + 192];
    int n = 0;

    frame[n++] = 0x81;  // FIN + text opcode
    if (len < 126) {
        frame[n++] = (uint8_t)len;
    } else if (len < 65536) {
        frame[n++] = 126;
        frame[n++] = (uint8_t)(len >> 8);
        frame[n++] = (uint8_t)(len & 0xFF);
    } else {
        return false;
    }
    memcpy(frame + n, payload, (size_t)len);
    n += len;

    return send(fd, frame, (size_t)n, MSG_DONTWAIT) == n;
}

// Every edge that accrued since the last tick travels in one frame, in order, as [[t,down],...].
// One frame per tick instead of one per element keeps the send path shallow, and a client is
// never dropped for a transient refusal — losing a KEY-up that way latches the browser's tone
// on, which is far worse than a frame or two of jitter.
static void ws_task(void *arg)
{
    (void)arg;

    for (;;) {
        uint32_t ms[8];
        bool down[8];
        int n = 0;

        while (n < 8 && edge_pop(&ms[n], &down[n])) {
            n++;
        }

        if (n > 0) {
            char buf[192];
            int len = snprintf(buf, sizeof(buf), "[");
            for (int e = 0; e < n && len < 170; e++) {
                len += snprintf(buf + len, sizeof(buf) - (size_t)len, "%s[%lu,%u]", e ? "," : "",
                                (unsigned long)ms[e], down[e] ? 1u : 0u);
            }
            snprintf(buf + len, sizeof(buf) - (size_t)len, "]");

            for (int i = 0; i < WS_CLIENTS; i++) {
                const int fd = s_clients[i];
                if (fd < 0 || esp_timer_get_time() < s_ready_us[i]) {
                    continue;
                }

                if (ws_send_text(fd, buf, (int)strlen(buf))) {
                    s_ws_fail[i] = 0;
                } else if (++s_ws_fail[i] >= WS_STALL_LIMIT) {
                    ESP_LOGW(TAG, "sidetone client %d stopped draining, dropping it", fd);
                    ws_drop_client(fd);
                    // Free the server's socket too, not just our slot: a page that reloads
                    // without a clean close would otherwise hold one of only seven sockets.
                    shutdown(fd, SHUT_RDWR);
                }
            }
        }
        vTaskDelay(pdMS_TO_TICKS(4));
    }
}

static const char INDEX_HTML[] = R"html(<!doctype html>
<html lang='en'><head><meta charset='utf-8'>
<meta name='viewport' content='width=device-width,initial-scale=1'>
<title>W1VE Remote Keyer</title>
<style>
:root{color-scheme:dark}
*{box-sizing:border-box}
body{margin:0;background:#0b0b0c;color:#e8e8ea;font:15px/1.5 system-ui,-apple-system,'Segoe UI',sans-serif}
header{padding:16px 18px 12px;border-bottom:1px solid #1e1e22}
h1{margin:0;font-size:17px;font-weight:600}
.sub{color:#8a8a92;font-size:13px;margin-top:4px}
main{max-width:640px;margin:0 auto;padding:14px 18px 32px}
.card{background:#131317;border:1px solid #1e1e22;border-radius:10px;padding:14px;margin-bottom:12px}
.card h2{margin:0 0 12px;font-size:11px;letter-spacing:.09em;text-transform:uppercase;color:#8a8a92}
.row{display:flex;align-items:center;justify-content:space-between;gap:14px}
.big{font-size:34px;font-variant-numeric:tabular-nums;line-height:1}
input[type=range]{width:100%;accent-color:#2f6fe0}
.seg{display:flex;gap:6px}
.seg button{flex:1;padding:10px 6px;border-radius:8px;border:1px solid #26262c;background:#1a1a1f;color:#c9c9cf;font:inherit;font-size:13px}
.seg button[aria-pressed=true]{background:#2f6fe0;border-color:#2f6fe0;color:#fff}
.dot{display:inline-block;width:11px;height:11px;border-radius:50%;background:#24242a;margin-right:8px;vertical-align:-1px}
.dot.k{background:#e03a3a}.dot.p{background:#2f6fe0}
button.act{padding:10px 16px;border-radius:8px;border:1px solid #26262c;background:#1a1a1f;color:#c9c9cf;font:inherit}
button.act.on{background:#2f6fe0;border-color:#2f6fe0;color:#fff}
.seg button:disabled,input[type=range]:disabled{opacity:.45}
</style></head><body>
<header><h1>W1VE Remote Keyer</h1><div class='sub' id='sub'>connecting&hellip;</div></header>
<main>
<div class='card'><h2>Network</h2><div class='row'><button class='act' id='scan'>Scan</button><span class='sub' id='ns'>not scanned</span></div><div id='nets' style='margin-top:10px'></div><div class='row' style='margin-top:10px'><input id='ssid' placeholder='SSID' style='flex:1;min-width:0;padding:9px;border-radius:8px;border:1px solid #26262c;background:#1a1a1f;color:#e8e8ea;font:inherit'><input id='pass' type='password' placeholder='Password' style='flex:1;min-width:0;padding:9px;border-radius:8px;border:1px solid #26262c;background:#1a1a1f;color:#e8e8ea;font:inherit'></div><div class='row' style='margin-top:10px'><button class='act' id='join'>Join network</button><span class='sub' id='js'></span></div></div>
<div class='card'><h2>Remote keyer</h2>
<div class='row'><span class='sub'>How a remote operator reaches this station. Pick one &mdash; LAN and WAN can't both be on.</span></div>
<div class='seg' id='rmode' style='margin-top:10px'><button data-r='off'>Off</button><button data-r='lan'>LAN</button><button data-r='wan'>Internet</button></div>
<div class='row' style='margin-top:6px'><span class='sub' id='rstate'>off</span></div>

<div id='lanbox' style='margin-top:12px;display:none'>
<div class='row'><span class='sub'>Station name (shown to Keyers browsing the LAN)</span></div>
<div class='row' style='margin-top:6px'><input id='pname' maxlength='32' placeholder='W1VE Keyer' style='flex:1;min-width:0;padding:9px;border-radius:8px;border:1px solid #26262c;background:#1a1a1f;color:#e8e8ea;font:inherit'>
<button class='act' id='savename'>Save</button></div>
<div class='row' style='margin-top:10px'><span class='sub' id='rs'>not paired</span>
<button class='act' id='unpair'>Unpair</button></div></div>

<div id='wanbox' style='margin-top:12px;display:none'>
<div class='row'><span class='sub'>Give this pairing code to the remote Keyer. Both ends enter the same code to connect through the relay. A random code is set per device; change it to anything, or clear and Save to roll a new one.</span></div>
<div class='row' style='margin-top:6px'><input id='wcode' maxlength='32' placeholder='(random)' style='flex:1;min-width:0;padding:11px;border-radius:8px;border:1px solid #26262c;background:#1a1a1f;color:#e8e8ea;font:inherit;font-size:18px;font-weight:700;letter-spacing:1px'>
<button class='act' id='wcodesave'>Save</button>
<button class='act' id='wcodecopy' title='Copy'>Copy</button></div>
<div class='row' style='margin-top:10px'><span class='sub'>Relay</span>
<input id='wrelay' placeholder='host:port' style='flex:1;min-width:0;padding:9px;border-radius:8px;border:1px solid #26262c;background:#1a1a1f;color:#e8e8ea;font:inherit'>
<button class='act' id='wrelaysave'>Save</button></div>
<div class='row' style='margin-top:10px'><span class='sub' id='wstate'>WAN off</span>
<button class='act' id='wdisc'>Disconnect</button></div>
<div class='row' style='margin-top:10px'><span class='sub'>This station's key fingerprint (for verification): <span id='wkey' style='font-family:monospace'>&mdash;</span></span></div></div></div>
<div class='card'><h2>Line</h2><div class='row'><span><i class='dot' id='kd'></i>KEY</span><span><i class='dot' id='pd'></i>PTT</span></div></div>
<div class='card'><h2>Speed</h2><div class='row'><span class='big' id='big'>--</span><span>WPM</span></div><input id='wpm' type='range' min='5' max='50' step='1'><div class='sub' id='shint' style='margin-top:8px'></div></div>
<div class='card'><h2>Mode</h2><div class='seg' id='mode'><button data-m='0'>Iambic A</button><button data-m='1'>Iambic B</button><button data-m='2'>Straight</button></div></div>
<div class='card'><h2>Weight</h2><div class='row'><span class='big' id='wbig'>50%</span><span class='sub'>50% is even, in steps of 1%</span></div><input id='weight' type='range' min='25' max='75' step='1' value='50'></div>
<div class='card'><h2>Macros</h2><div id='macros'><span class='sub'>loading&hellip;</span></div></div>
<div class='card'><h2>Sidetone</h2><div class='row'><button class='act' id='tone'>Enable sidetone</button><button class='act' id='kt'>Test key</button><span class='sub' id='ts'>off</span></div><div class='row' style='margin-top:14px'><span>Tone</span><input id='freq' type='range' min='400' max='1000' step='10' value='650' style='max-width:62%'></div><div class='row'><span>Volume</span><input id='vol' type='range' min='0' max='100' step='1' value='55' style='max-width:62%'></div></div>
</main>
<script>
var $=function(i){return document.getElementById(i);};
function post(u,b){return fetch(u,{method:'POST',headers:{'Content-Type':'application/x-www-form-urlencoded'},body:b});}
function sync(){fetch('/api/status').then(function(r){return r.json();}).then(function(s){$('sub').textContent=s.network+' \u00b7 '+s.address+' / jitter '+s.jitter+' us'+(s.fw?(' \u00b7 v'+s.fw):'');$('big').textContent=s.speed;var locked=!!s.remote;if(!locked&&document.activeElement!==$('wpm'))$('wpm').value=s.wpm;$('wpm').disabled=locked;$('shint').textContent=locked?'speed and mode are set by the remote Keyer':'';$('wbig').textContent=s.weight+'%';if(document.activeElement!==$('weight'))$('weight').value=s.weight;var k=$('mode').children;for(var i=0;i<k.length;i++){k[i].setAttribute('aria-pressed',(+k[i].dataset.m===s.mode)?'true':'false');k[i].disabled=locked;}$('kd').className='dot'+(s.key?' k':'');$('pd').className='dot'+(s.ptt?' p':'');st=s;$('rs').textContent=s.peer_paired?(s.peer+' / '+(s.peer_wpm?s.peer_wpm+' WPM / ':'')+(s.session?'keying':(s.peer_alive?'online':'offline'))+' / rtt '+s.rtt+' ms'):(s.reason?s.reason:'not paired');
var w=s.wan||{};
// Exactly one transport is the active mode. WAN wins the label when it is on, since the two are
// mutually exclusive; otherwise LAN-on or Off.
var mode=w.enabled?'wan':(s.lan?'lan':'off');
var rb=$('rmode').children;for(var j=0;j<rb.length;j++){rb[j].setAttribute('aria-pressed',(rb[j].dataset.r===mode)?'true':'false');}
$('lanbox').style.display=(mode==='lan')?'block':'none';
$('wanbox').style.display=(mode==='wan')?'block':'none';
// One-line summary of the chosen mode.
var rtxt;
if(mode==='off'){rtxt='remote keying is off';}
else if(mode==='lan'){rtxt=s.peer_paired?('LAN paired with '+s.keyer):(s.lan?('LAN: advertising as '+s.name):'LAN');}
else{rtxt='Internet: '+w.state+(w.active?(' '+(w.direct?'(direct)':'(relay)')):'');}
$('rstate').textContent=rtxt;
// LAN detail line.
$('rs').textContent=s.peer_paired?(s.peer+' / '+(s.peer_wpm?s.peer_wpm+' WPM / ':'')+(s.session?'keying':(s.peer_alive?'online':'offline'))+' / rtt '+s.rtt+' ms'):(s.reason?s.reason:'not paired');
// WAN detail.
if(document.activeElement!==$('wrelay'))$('wrelay').value=w.relay||'';
if(document.activeElement!==$('wcode'))$('wcode').value=w.code||'';
var wtxt;
if(!w.enabled){wtxt='off';}
else{wtxt=w.state+(w.active?(' '+(w.direct?'(direct)':'(relay)')+' \u00b7 '+(w.peer_wpm?w.peer_wpm+' WPM \u00b7 ':'')+'rtt '+w.rtt+' ms \u00b7 jitter '+w.jitter+' ms'):'')+(w.reason&&!w.active?(' \u00b7 '+w.reason):'');}
$('wstate').textContent=wtxt;
$('wkey').textContent=w.key||'\u2014';
}).catch(function(){$('sub').textContent='disconnected';});}
setInterval(sync,400);sync();var st={};
$('unpair').addEventListener('click',function(){post('/api/unpair','').then(function(){sync();});});
$('savename').addEventListener('click',function(){post('/api/identity','name='+encodeURIComponent($('pname').value.trim())).then(function(){sync();});});
// The mode selector enforces one transport at a time: choosing LAN turns WAN off and vice versa,
// so they can never both run. Off stops both.
$('rmode').addEventListener('click',function(e){var b=e.target.closest('button');if(!b)return;var r=b.dataset.r;
if(r==='off'){post('/api/wan','on=0');post('/api/advertise','on=0');}
else if(r==='lan'){post('/api/wan','on=0').then(function(){return post('/api/advertise','on=1');});}
else{post('/api/advertise','on=0').then(function(){return post('/api/wan','on=1');});}
setTimeout(sync,200);});
$('wrelaysave').addEventListener('click',function(){post('/api/wan/relay','relay='+encodeURIComponent($('wrelay').value.trim())).then(function(){sync();});});
$('wcodesave').addEventListener('click',function(){post('/api/wan/code','code='+encodeURIComponent($('wcode').value.trim())).then(function(){sync();});});
$('wcodecopy').addEventListener('click',function(){var v=$('wcode').value.trim();if(navigator.clipboard&&v){navigator.clipboard.writeText(v).then(function(){$('wcodecopy').textContent='Copied';setTimeout(function(){$('wcodecopy').textContent='Copy';},1200);});}});
$('wdisc').addEventListener('click',function(){post('/api/wan/disconnect','').then(function(){sync();});});
fetch('/api/status').then(function(r){return r.json();}).then(function(s){$('pname').value=s.name;});
$('wpm').addEventListener('input',function(e){if(st.remote)return;$('big').textContent=e.target.value;post('/api/wpm','wpm='+e.target.value);});
$('weight').addEventListener('input',function(e){$('wbig').textContent=e.target.value+'%';post('/api/weight','weight='+e.target.value);});
$('mode').addEventListener('click',function(e){if(st.remote)return;var b=e.target.closest('button');if(!b)return;post('/api/mode','mode='+b.dataset.m);setTimeout(sync,120);});
var ctx=null,master=null,tone=null,osc=null,ws=null,anchor=0,lastT=0,lastAt=0,heldAt=-1e9;
// How far ahead of the audio clock a run of edges is scheduled. Anything scheduled is heard one
// output latency after currentTime, so a big lead here is a lag you can hear against the KEY LED;
// 50 ms covers Wi-Fi arrival jitter, and a frame that misses it is played at once rather than
// waiting for a slot.
var LEAD=0.05,RAMP=0.002;
function audio(){if(ctx)return;ctx=new (window.AudioContext||window.webkitAudioContext)();
master=ctx.createGain();master.gain.value=$('vol').value/100;master.connect(ctx.destination);
osc=ctx.createOscillator();osc.type='sine';osc.frequency.value=+$('freq').value;
tone=ctx.createGain();tone.gain.value=0;
osc.connect(tone);tone.connect(master);osc.start();}
function silent(t){if(!ctx||!tone)return;tone.gain.cancelScheduledValues(t);
tone.gain.setValueAtTime(0,t);}
function reset(){anchor=0;lastT=0;lastAt=0;if(ctx&&tone)silent(ctx.currentTime);}
function edge(down,at){if(!ctx||!tone)return;
if(at<lastAt+0.001)at=lastAt+0.001;
if(at<ctx.currentTime+0.004)at=ctx.currentTime+0.004;
lastAt=at;
var g=tone.gain;g.cancelScheduledValues(at);
g.setValueAtTime(down?0:1,at);
g.linearRampToValueAtTime(down?1:0,at+RAMP);
if(down)heldAt=at;}
function show(){if(!ctx)return;var l=Math.round((lastAt-ctx.currentTime)*1000);
$('ts').textContent=(ctx.state==='running'?'':'suspended ')+'lead '+l+' ms';}
function wake(){if(!ctx||ctx.state==='running')return;
ctx.resume().then(function(){reset();show();},function(){show();});}
function connect(){ws=new WebSocket((location.protocol==='https:'?'wss://':'ws://')+location.host+'/ws');
ws.onopen=function(){reset();$('ts').textContent='ready';};
ws.onmessage=function(ev){var m=JSON.parse(ev.data);if(!ctx||!m.length)return;wake();
var t0=m[0][0],now=ctx.currentTime;
// Re-map device time onto the audio clock only when the stream breaks: first frame, an idle
// gap, a device clock jump, or a mapping so stale the tone would arrive late. The mapping then
// holds for the rest of the run, so one straggling frame plays late by its own delay instead of
// shifting every element after it.
if(!anchor||t0+600<lastT||t0>lastT+900000||t0-lastT>1500||anchor+t0/1000<now-0.15){anchor=now+LEAD-t0/1000;lastAt=0;lastT=t0-1;}
for(var i=0;i<m.length;i++){if(m[i][0]<=lastT)continue;lastT=m[i][0];edge(m[i][1],anchor+m[i][0]/1000);}
show();};
ws.onclose=function(){ws=null;reset();
if($('tone').classList.contains('on'))setTimeout(connect,800);};}
document.addEventListener('visibilitychange',function(){if(!document.hidden)wake();});
setInterval(function(){wake();if(ctx&&tone&&ctx.currentTime-heldAt>1.5)silent(ctx.currentTime);},500);
$('tone').addEventListener('click',function(){audio();wake();
if($('tone').classList.contains('on')){$('tone').classList.remove('on');
$('tone').textContent='Enable sidetone';$('ts').textContent='off';
if(ws){var w=ws;ws=null;w.close();}silent(ctx.currentTime);}
else{$('tone').classList.add('on');$('tone').textContent='Sidetone on';
$('ts').textContent='connecting'+String.fromCharCode(8230);connect();}});
$('freq').addEventListener('input',function(){if(osc)osc.frequency.value=+$('freq').value;});
$('vol').addEventListener('input',function(){if(master)master.gain.value=$('vol').value/100;});
$('kt').addEventListener('click',function(){audio();wake();post('/api/keytest','elements=12').then(function(){show();});});
function macroRows(list){var box=$('macros');box.innerHTML='';
for(var i=0;i<list.length;i++){(function(i){
var row=document.createElement('div');row.className='row';row.style.marginTop='8px';
var lab=document.createElement('input');lab.id='ml'+i;lab.value=list[i].label;lab.maxLength=10;lab.placeholder='Label';lab.style.width='78px';lab.style.padding='9px';lab.style.borderRadius='8px';lab.style.border='1px solid #26262c';lab.style.background='#1a1a1f';lab.style.color='#e8e8ea';lab.style.font='inherit';
var txt=document.createElement('input');txt.id='mt'+i;txt.value=list[i].text;txt.maxLength=48;txt.placeholder='(Empty)';txt.style.flex='1';txt.style.minWidth='0';txt.style.padding='9px';txt.style.borderRadius='8px';txt.style.border='1px solid #26262c';txt.style.background='#1a1a1f';txt.style.color='#e8e8ea';txt.style.font='inherit';
var send=document.createElement('button');send.className='act';send.textContent='Send';
send.onclick=function(){fetch('/api/macro/play',{method:'POST',headers:{'Content-Type':'application/x-www-form-urlencoded'},body:'idx='+i});};
var b=document.createElement('button');b.className='act';b.textContent='Save';
b.onclick=function(){fetch('/api/macro',{method:'POST',headers:{'Content-Type':'application/x-www-form-urlencoded'},body:'idx='+i+'&label='+encodeURIComponent($('ml'+i).value)+'&text='+encodeURIComponent($('mt'+i).value)}).then(function(r){txt.style.borderColor=r.ok?'#3ACF6A':'#E03A3A';setTimeout(function(){txt.style.borderColor='#26262c';},700);}).catch(function(){txt.style.borderColor='#E03A3A';});};
row.appendChild(lab);row.appendChild(txt);row.appendChild(send);row.appendChild(b);box.appendChild(row);})(i);}}
fetch('/api/macros').then(function(r){return r.json();}).then(macroRows).catch(function(){$('macros').textContent='unavailable';});
$('scan').addEventListener('click',function(){$('ns').textContent='scanning\u2026';fetch('/api/scan').then(function(r){return r.json();}).then(function(list){var box=$('nets');box.innerHTML='';if(!list.length){$('ns').textContent='none found';return;}$('ns').textContent=list.length+' found';list.forEach(function(n){var b=document.createElement('button');b.className='act';b.style.margin='0 6px 6px 0';b.style.padding='7px 10px';b.style.fontSize='13px';b.textContent=n.ssid+(n.secure?' \u00b7 locked':'')+'  '+n.rssi;b.onclick=function(){$('ssid').value=n.ssid;};box.appendChild(b);});}).catch(function(){$('ns').textContent='scan failed';});});
$('join').addEventListener('click',function(){$('js').textContent='joining\u2026';post('/api/join','ssid='+encodeURIComponent($('ssid').value)+'&pass='+encodeURIComponent($('pass').value)).then(function(r){$('js').textContent=r.ok?'check the header for the new address':'failed';sync();}).catch(function(){$('js').textContent='failed';});});
</script></body></html>
)html";


// ------------------------------------------------------------------ request helpers

static bool read_body(httpd_req_t *req, char *buf, size_t len)
{
    if (req->content_len == 0 || req->content_len >= len) {
        return false;
    }
    int received = 0;
    while (received < (int)req->content_len) {
        const int r = httpd_req_recv(req, buf + received, req->content_len - received);
        if (r <= 0) {
            return false;
        }
        received += r;
    }
    buf[received] = '\0';
    return true;
}

static int form_int(const char *body, const char *key, int fallback)
{
    const char *p = strstr(body, key);
    if (p == NULL) {
        return fallback;
    }
    return atoi(p + strlen(key));
}

static void url_decode(char *dst, size_t len, const char *src)
{
    size_t o = 0;
    for (size_t i = 0; src[i] != '\0' && o + 1 < len; i++) {
        if (src[i] == '+') {
            dst[o++] = ' ';
        } else if (src[i] == '%' && src[i + 1] != '\0' && src[i + 2] != '\0') {
            const char hex[3] = {src[i + 1], src[i + 2], '\0'};
            dst[o++] = (char)strtol(hex, NULL, 16);
            i += 2;
        } else {
            dst[o++] = src[i];
        }
    }
    dst[o] = '\0';
}

// Copy one form field out of the body, URL-decoded. Returns false when the key is absent.
static bool form_field(const char *body, const char *key, char *out, size_t len)
{
    const char *p = strstr(body, key);
    if (p == NULL) {
        return false;
    }
    p += strlen(key);

    char raw[192];
    size_t n = 0;
    while (*p != '\0' && *p != '&' && n + 1 < sizeof(raw)) {
        raw[n++] = *p++;
    }
    raw[n] = '\0';
    url_decode(out, len, raw);
    return true;
}

// JSON string escaping for the status and scan responses.
static void json_escape(const char *src, char *out, size_t len)
{
    size_t o = 0;
    for (size_t i = 0; src[i] != '\0' && o + 2 < len; i++) {
        const unsigned char c = (unsigned char)src[i];
        if (c == '"' || c == '\\') {
            out[o++] = '\\';
            out[o++] = (char)c;
        } else if (c < 0x20) {
            out[o++] = ' ';
        } else {
            out[o++] = (char)c;
        }
    }
    out[o] = '\0';
}

// ------------------------------------------------------------------ handlers

static esp_err_t index_get(httpd_req_t *req)
{
    httpd_resp_set_type(req, "text/html; charset=utf-8");
    httpd_resp_set_hdr(req, "Cache-Control", "no-store");
    return httpd_resp_send(req, INDEX_HTML, HTTPD_RESP_USE_STRLEN);
}

static esp_err_t status_get(httpd_req_t *req)
{
    const net_status_t net = net_get_status();
    const keyer_status_t k = keyer_get_status();
    const remote_status_t rem = remote_get_status();
    const remote_identity_t ident = remote_get_identity();

    char network[40];
    if (net.connected) {
        snprintf(network, sizeof(network), "%s", net.ssid);
    } else {
        snprintf(network, sizeof(network), "AP %s", net.ap_ssid);
    }

    char netname[sizeof(network) * 2];
    json_escape(network, netname, sizeof(netname));

    char name[REMOTE_NAME_MAX * 2 + 2];
    char pid[REMOTE_KEYER_LEN * 2];
    char peer[REMOTE_PEER_LEN * 2];
    char reason[REMOTE_REASON_LEN * 2];
    char keyer[REMOTE_KEYER_LEN * 2];
    json_escape(ident.name, name, sizeof(name));
    json_escape(ident.player_id, pid, sizeof(pid));
    json_escape(rem.peer, peer, sizeof(peer));
    json_escape(rem.reason, reason, sizeof(reason));
    json_escape(rem.keyer, keyer, sizeof(keyer));

    // The effective remote view follows whichever transport owns the line. A WAN session that is
    // active takes precedence over the LAN fields for the single remote readout (speed, keying),
    // because it is the one actually driving the key.
    const wan_status_t wan = wan_get_status();
    const bool wan_active = wan.active;
    const bool remote = rem.active || wan_active;
    unsigned speed;
    if (wan_active && wan.peer_wpm > 0) {
        speed = wan.peer_wpm;
    } else if (rem.active && rem.peer_wpm > 0) {
        speed = rem.peer_wpm;
    } else {
        speed = (unsigned)k.wpm;
    }

    char wan_relay[WAN_RELAY_MAX * 2 + 2];
    char wan_code[WAN_CODE_MAX * 2 + 2];
    char wan_reason[WAN_REASON_LEN * 2];
    json_escape(wan.relay, wan_relay, sizeof(wan_relay));
    json_escape(wan.code, wan_code, sizeof(wan_code));
    json_escape(wan.reason, wan_reason, sizeof(wan_reason));

    char body[1100];
    int used = snprintf(body, sizeof(body),
             "{\"fw\":\"%s\",\"network\":\"%s\",\"address\":\"%s\",\"wpm\":%u,\"mode\":%u,"
             "\"weight\":%u,\"key\":%s,\"ptt\":%s,\"jitter\":%u,\"remote\":%s,"
             "\"speed\":%u,"
             "\"name\":\"%s\",\"pid\":\"%s\",\"lan\":%s,\"keyer\":\"%s\","
             "\"peer\":\"%s\",\"peer_alive\":%s,\"peer_paired\":%s,\"peer_wpm\":%u,"
             "\"session\":%s,\"rtt\":%d,\"reason\":\"%s\"",
             W1VE_FW_VERSION, netname, net.address, (unsigned)k.wpm,
             (unsigned)k.mode, (unsigned)k.weight, k.key_down ? "true" : "false",
             k.ptt_down ? "true" : "false", (unsigned)keyer_take_max_jitter_us(),
             remote ? "true" : "false", speed, name, pid,
             ident.advertising ? "true" : "false", keyer, rem.paired ? peer : "",
             rem.alive ? "true" : "false", rem.paired ? "true" : "false",
             (unsigned)rem.peer_wpm, rem.keying ? "true" : "false", rem.rtt_ms, reason);
    snprintf(body + used, sizeof(body) - (size_t)used,
             ",\"wan\":{\"enabled\":%s,\"state\":\"%s\",\"paired\":%s,\"active\":%s,"
             "\"keying\":%s,\"direct\":%s,\"relay\":\"%s\",\"code\":\"%s\",\"key\":\"%s\","
             "\"peer_wpm\":%u,\"rtt\":%d,\"jitter\":%u,\"reason\":\"%s\"}}",
             wan.enabled ? "true" : "false", wan_state_name(wan.state),
             wan.paired ? "true" : "false", wan_active ? "true" : "false",
             wan.keying ? "true" : "false", wan.direct ? "true" : "false", wan_relay, wan_code,
             wan.pubkey_fp, (unsigned)wan.peer_wpm, wan.rtt_ms, (unsigned)wan.jitter_ms,
             wan_reason);

    httpd_resp_set_type(req, "application/json");
    httpd_resp_set_hdr(req, "Cache-Control", "no-store");
    return httpd_resp_send(req, body, HTTPD_RESP_USE_STRLEN);
}

static esp_err_t scan_get(httpd_req_t *req)
{
    net_scan_result_t res = {0};
    char body[768];
    int used = snprintf(body, sizeof(body), "[");

    if (net_scan(&res)) {
        for (int i = 0; i < res.count; i++) {
            char safe[80];
            json_escape(res.entries[i].ssid, safe, sizeof(safe));
            const int room = (int)sizeof(body) - used;
            if (room < 100) {
                break;
            }
            used += snprintf(body + used, (size_t)room,
                             "%s{\"ssid\":\"%s\",\"rssi\":%d,\"secure\":%s}", i ? "," : "", safe,
                             res.entries[i].rssi, res.entries[i].secure ? "true" : "false");
        }
    }
    snprintf(body + used, sizeof(body) - (size_t)used, "]");

    httpd_resp_set_type(req, "application/json");
    httpd_resp_set_hdr(req, "Cache-Control", "no-store");
    return httpd_resp_send(req, body, HTTPD_RESP_USE_STRLEN);
}

static esp_err_t join_post(httpd_req_t *req)
{
    char body[256];
    if (!read_body(req, body, sizeof(body))) {
        return httpd_resp_send_408(req);
    }

    char ssid[33] = {0};
    char pass[65] = {0};
    if (!form_field(body, "ssid=", ssid, sizeof(ssid)) || ssid[0] == '\0') {
        return httpd_resp_send_err(req, HTTPD_400_BAD_REQUEST, "ssid required");
    }
    form_field(body, "pass=", pass, sizeof(pass));

    // Blocks up to ~12 s while the station tries to associate; the page shows 'joining'.
    const bool ok = net_join(ssid, pass);
    ESP_LOGI(TAG, "join '%s': %s", ssid, ok ? "ok" : "failed");
    return httpd_resp_sendstr(req, ok ? "joined" : "failed");
}

static esp_err_t wpm_post(httpd_req_t *req)
{
    char body[64];
    if (!read_body(req, body, sizeof(body))) {
        return httpd_resp_send_408(req);
    }
    const int wpm = form_int(body, "wpm=", -1);
    if (wpm >= KEYER_WPM_MIN && wpm <= KEYER_WPM_MAX) {
        keyer_set_wpm((uint8_t)wpm);
    }
    return httpd_resp_sendstr(req, "ok");
}

static esp_err_t weight_post(httpd_req_t *req)
{
    char body[64];
    if (!read_body(req, body, sizeof(body))) {
        return httpd_resp_send_408(req);
    }
    const int weight = form_int(body, "weight=", -1);
    if (weight >= KEYER_WEIGHT_MIN && weight <= KEYER_WEIGHT_MAX) {
        keyer_set_weight((uint8_t)weight);
    }
    return httpd_resp_sendstr(req, "ok");
}

static esp_err_t mode_post(httpd_req_t *req)
{
    char body[64];
    if (!read_body(req, body, sizeof(body))) {
        return httpd_resp_send_408(req);
    }
    const int mode = form_int(body, "mode=", -1);
    if (mode >= 0 && mode < KEYER_MODE_COUNT) {
        keyer_set_mode((keyer_mode_t)mode);
    }
    return httpd_resp_sendstr(req, "ok");
}

static esp_err_t identity_post(httpd_req_t *req)
{
    char body[192];
    if (!read_body(req, body, sizeof(body))) {
        return httpd_resp_send_408(req);
    }

    char name[REMOTE_NAME_MAX];
    if (!form_field(body, "name=", name, sizeof(name)) || name[0] == '\0' ||
        strlen(name) >= REMOTE_NAME_MAX - 1) {
        return httpd_resp_send_err(req, HTTPD_400_BAD_REQUEST, "name required");
    }
    remote_set_name(name);
    return httpd_resp_sendstr(req, "saved");
}

static esp_err_t unpair_post(httpd_req_t *req)
{
    (void)req->content_len;
    remote_unpair();
    return httpd_resp_sendstr(req, "ok");
}

static esp_err_t advertise_post(httpd_req_t *req)
{
    char body[64];
    if (!read_body(req, body, sizeof(body))) {
        return httpd_resp_send_408(req);
    }

    char on[4];
    const bool got = form_field(body, "on=", on, sizeof(on));
    remote_set_advertising(got && on[0] == '1');
    return httpd_resp_sendstr(req, "ok");
}

// ------------------------------------------------------------------ WAN

static esp_err_t wan_enable_post(httpd_req_t *req)
{
    char body[64];
    if (!read_body(req, body, sizeof(body))) {
        return httpd_resp_send_408(req);
    }
    char on[4];
    const bool got = form_field(body, "on=", on, sizeof(on));
    wan_set_enabled(got && on[0] == '1');
    return httpd_resp_sendstr(req, "ok");
}

static esp_err_t wan_relay_post(httpd_req_t *req)
{
    char body[192];
    if (!read_body(req, body, sizeof(body))) {
        return httpd_resp_send_408(req);
    }
    char relay[WAN_RELAY_MAX + 1] = {0};
    form_field(body, "relay=", relay, sizeof(relay));  // empty restores the default
    wan_set_relay(relay);
    return httpd_resp_sendstr(req, "saved");
}

static esp_err_t wan_code_post(httpd_req_t *req)
{
    char body[128];
    if (!read_body(req, body, sizeof(body))) {
        return httpd_resp_send_408(req);
    }
    char code[WAN_CODE_MAX + 1] = {0};
    form_field(body, "code=", code, sizeof(code));
    wan_set_code(code);
    return httpd_resp_sendstr(req, "saved");
}

static esp_err_t wan_disconnect_post(httpd_req_t *req)
{
    (void)req->content_len;
    wan_disconnect();
    return httpd_resp_sendstr(req, "ok");
}

// Drives the status LED straight from the page, so the part and its wiring can be proven without
// waiting for a network change. The colour stays until the next transport state change.
static esp_err_t led_post(httpd_req_t *req)
{
    char body[64];
    if (!read_body(req, body, sizeof(body))) {
        return httpd_resp_send_408(req);
    }
    const int r = form_int(body, "r=", -1);
    const int g = form_int(body, "g=", -1);
    const int b = form_int(body, "b=", -1);
    if (r < 0 || g < 0 || b < 0 || r > 255 || g > 255 || b > 255) {
        return httpd_resp_send_err(req, HTTPD_400_BAD_REQUEST, "r, g and b are 0..255");
    }
    board_led_set((uint8_t)r, (uint8_t)g, (uint8_t)b);
    ESP_LOGI(TAG, "LED test: r=%u g=%u b=%u", (unsigned)r, (unsigned)g, (unsigned)b);
    return httpd_resp_sendstr(req, "ok");
}

static void keytest_task(void *arg)
{
    const uint16_t elements = (uint16_t)(uintptr_t)arg;
    if (!keyer_keytest(elements)) {
        ESP_LOGW(TAG, "key test refused: the line is busy");
    }
    vTaskDelete(NULL);
}

// Deliberate operator action: keys the transmitter. The page's Test key button posts here, so
// the whole chain — element machine, KEY pad, edge stream, browser tone — can be proven with no
// Keyer at the other end.
static esp_err_t keytest_post(httpd_req_t *req)
{
    char body[64];
    if (!read_body(req, body, sizeof(body))) {
        return httpd_resp_send_408(req);
    }

    int elements = form_int(body, "elements=", 6);
    if (elements < 1) {
        elements = 1;
    }
    if (elements > 30) {
        elements = 30;
    }

    // Runs for a second or two, so never on the HTTP task.
    if (xTaskCreate(keytest_task, "keytest", 3072, (void *)(uintptr_t)elements, 4, NULL) != pdPASS) {
        return httpd_resp_send_err(req, HTTPD_500_INTERNAL_SERVER_ERROR, "no task");
    }
    return httpd_resp_sendstr(req, "ok");
}

static esp_err_t macros_get(httpd_req_t *req)
{
    char out[1024];
    int len = snprintf(out, sizeof(out), "[");
    for (int i = 0; i < macro_slots() && len < (int)sizeof(out) - 160; i++) {
        char label[MACRO_LABEL_MAX];
        char text[MACRO_TEXT_MAX];
        macro_get(i, label, sizeof(label), text, sizeof(text));

        char lsafe[2 * MACRO_LABEL_MAX];
        char tsafe[2 * MACRO_TEXT_MAX];
        json_escape(label, lsafe, sizeof(lsafe));
        json_escape(text, tsafe, sizeof(tsafe));

        len += snprintf(out + len, sizeof(out) - (size_t)len,
                        "%s{\"label\":\"%s\",\"text\":\"%s\"}", i ? "," : "", lsafe, tsafe);
    }
    snprintf(out + len, sizeof(out) - (size_t)len, "]");

    httpd_resp_set_type(req, "application/json");
    return httpd_resp_sendstr(req, out);
}

static esp_err_t macro_play_post(httpd_req_t *req)
{
    char body[64];
    if (!read_body(req, body, sizeof(body))) {
        return httpd_resp_send_408(req);
    }
    const int index = form_int(body, "idx=", -1);
    if (index < 0 || index >= macro_slots()) {
        return httpd_resp_send_err(req, HTTPD_400_BAD_REQUEST, "idx required");
    }
    if (!macro_play(index)) {
        httpd_resp_set_status(req, "409 Conflict");
        return httpd_resp_sendstr(req, "line busy or slot empty");
    }
    return httpd_resp_sendstr(req, "sending");
}

// The page edits one slot at a time; both fields travel together so a save can never leave a
// label pointing at the text that used to be next to it.
static esp_err_t macro_post(httpd_req_t *req)
{
    char body[256];
    if (!read_body(req, body, sizeof(body))) {
        return httpd_resp_send_408(req);
    }

    const int index = form_int(body, "idx=", -1);
    char label[MACRO_LABEL_MAX];
    char text[MACRO_TEXT_MAX];
    if (index < 0 || index >= macro_slots() || !form_field(body, "label=", label, sizeof(label)) ||
        !form_field(body, "text=", text, sizeof(text))) {
        return httpd_resp_send_err(req, HTTPD_400_BAD_REQUEST, "idx, label and text required");
    }

    if (!macro_set(index, label, text)) {
        return httpd_resp_send_err(req, HTTPD_500_INTERNAL_SERVER_ERROR, "save failed");
    }
    return httpd_resp_sendstr(req, "saved");
}

static esp_err_t ws_handler(httpd_req_t *req)
{
    if (req->method == HTTP_GET) {
        ws_add_client(httpd_req_to_sockfd(req));
        ESP_LOGI(TAG, "sidetone client connected");
        return ESP_OK;
    }

    // Nothing is expected from the browser; drain it so the connection stays healthy.
    httpd_ws_frame_t frame = {0};
    frame.type = HTTPD_WS_TYPE_TEXT;
    uint8_t buf[64] = {0};
    frame.payload = buf;
    return httpd_ws_recv_frame(req, &frame, sizeof(buf) - 1);
}

// One loopback request at boot, so the log states plainly whether port 80 answers rather
// than leaving it to be discovered from a phone.
static void self_test_task(void *arg)
{
    (void)arg;
    vTaskDelay(pdMS_TO_TICKS(1500));

    const int sock = socket(AF_INET, SOCK_STREAM, IPPROTO_IP);
    if (sock < 0) {
        ESP_LOGW(TAG, "self-test: socket failed");
        vTaskDelete(NULL);
        return;
    }

    struct sockaddr_in addr = {0};
    addr.sin_family = AF_INET;
    addr.sin_port = htons(80);
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);

    const struct timeval timeout = {.tv_sec = 3, .tv_usec = 0};
    setsockopt(sock, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));

    if (connect(sock, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
        ESP_LOGW(TAG, "self-test: nothing listening on 80");
        close(sock);
        vTaskDelete(NULL);
        return;
    }

    const char *get = "GET /api/status HTTP/1.0\r\nHost: localhost\r\n\r\n";
    send(sock, get, strlen(get), 0);

    char buf[192] = {0};
    const int n = recv(sock, buf, sizeof(buf) - 1, 0);
    close(sock);

    if (n > 0) {
        char *eol = strchr(buf, '\r');
        if (eol != NULL) {
            *eol = '\0';
        }
        ESP_LOGI(TAG, "self-test: %s", buf);
    } else {
        ESP_LOGW(TAG, "self-test: no response");
    }
    vTaskDelete(NULL);
}

// ------------------------------------------------------------------ start

void web_init(void)
{
    httpd_config_t config = HTTPD_DEFAULT_CONFIG();
    config.max_uri_handlers = 24;
    config.max_open_sockets = 10;
    config.lru_purge_enable = true;
    config.stack_size = 8192;

    if (httpd_start(&s_server, &config) != ESP_OK) {
        ESP_LOGE(TAG, "httpd_start failed");
        return;
    }

    static const httpd_uri_t index_uri = {
        .uri = "/", .method = HTTP_GET, .handler = index_get, .user_ctx = NULL};
    static const httpd_uri_t status_uri = {
        .uri = "/api/status", .method = HTTP_GET, .handler = status_get, .user_ctx = NULL};
    static const httpd_uri_t wpm_uri = {
        .uri = "/api/wpm", .method = HTTP_POST, .handler = wpm_post, .user_ctx = NULL};
    static const httpd_uri_t mode_uri = {
        .uri = "/api/mode", .method = HTTP_POST, .handler = mode_post, .user_ctx = NULL};
    static const httpd_uri_t scan_uri = {
        .uri = "/api/scan", .method = HTTP_GET, .handler = scan_get, .user_ctx = NULL};
    static const httpd_uri_t join_uri = {
        .uri = "/api/join", .method = HTTP_POST, .handler = join_post, .user_ctx = NULL};
    static const httpd_uri_t weight_uri = {
        .uri = "/api/weight", .method = HTTP_POST, .handler = weight_post, .user_ctx = NULL};
    static const httpd_uri_t unpair_uri = {
        .uri = "/api/unpair", .method = HTTP_POST, .handler = unpair_post, .user_ctx = NULL};
    static const httpd_uri_t identity_uri = {
        .uri = "/api/identity", .method = HTTP_POST, .handler = identity_post, .user_ctx = NULL};
    static const httpd_uri_t advertise_uri = {
        .uri = "/api/advertise", .method = HTTP_POST, .handler = advertise_post, .user_ctx = NULL};
    static const httpd_uri_t keytest_uri = {
        .uri = "/api/keytest", .method = HTTP_POST, .handler = keytest_post, .user_ctx = NULL};
    static const httpd_uri_t macros_uri = {
        .uri = "/api/macros", .method = HTTP_GET, .handler = macros_get, .user_ctx = NULL};
    static const httpd_uri_t macro_play_uri = {
        .uri = "/api/macro/play", .method = HTTP_POST, .handler = macro_play_post, .user_ctx = NULL};
    static const httpd_uri_t macro_uri = {
        .uri = "/api/macro", .method = HTTP_POST, .handler = macro_post, .user_ctx = NULL};
    static const httpd_uri_t ws_uri = {
        .uri = "/ws", .method = HTTP_GET, .handler = ws_handler, .is_websocket = true};
    static const httpd_uri_t wan_enable_uri = {
        .uri = "/api/wan", .method = HTTP_POST, .handler = wan_enable_post, .user_ctx = NULL};
    static const httpd_uri_t wan_relay_uri = {
        .uri = "/api/wan/relay", .method = HTTP_POST, .handler = wan_relay_post, .user_ctx = NULL};
    static const httpd_uri_t wan_code_uri = {
        .uri = "/api/wan/code", .method = HTTP_POST, .handler = wan_code_post, .user_ctx = NULL};
    static const httpd_uri_t wan_disconnect_uri = {
        .uri = "/api/wan/disconnect", .method = HTTP_POST, .handler = wan_disconnect_post,
        .user_ctx = NULL};

    httpd_register_uri_handler(s_server, &index_uri);
    httpd_register_uri_handler(s_server, &status_uri);
    httpd_register_uri_handler(s_server, &wpm_uri);
    httpd_register_uri_handler(s_server, &mode_uri);
    httpd_register_uri_handler(s_server, &weight_uri);
    httpd_register_uri_handler(s_server, &scan_uri);
    httpd_register_uri_handler(s_server, &join_uri);
    httpd_register_uri_handler(s_server, &unpair_uri);
    httpd_register_uri_handler(s_server, &identity_uri);
    static const httpd_uri_t led_uri = {
        .uri = "/api/led", .method = HTTP_POST, .handler = led_post, .user_ctx = NULL};
    httpd_register_uri_handler(s_server, &led_uri);
    httpd_register_uri_handler(s_server, &advertise_uri);
    httpd_register_uri_handler(s_server, &keytest_uri);
    httpd_register_uri_handler(s_server, &macros_uri);
    httpd_register_uri_handler(s_server, &macro_uri);
    httpd_register_uri_handler(s_server, &macro_play_uri);
    httpd_register_uri_handler(s_server, &wan_enable_uri);
    httpd_register_uri_handler(s_server, &wan_relay_uri);
    httpd_register_uri_handler(s_server, &wan_code_uri);
    httpd_register_uri_handler(s_server, &wan_disconnect_uri);
    httpd_register_uri_handler(s_server, &ws_uri);

    keyer_set_edge_cb(edge_push);
    xTaskCreate(ws_task, "sidetone", 4096, NULL, 3, NULL);
    xTaskCreate(self_test_task, "web_selftest", 3072, NULL, 2, NULL);

    ESP_LOGI(TAG, "web UI on :80, sidetone on /ws");
}
