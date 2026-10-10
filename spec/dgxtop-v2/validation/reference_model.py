"""規格的合成參考模型；不是 Rust 實作、硬體 Adapter 或安全驗證工具。"""
from dataclasses import dataclass
from math import isfinite
from typing import Optional

@dataclass(frozen=True)
class Candidate:
    source: str
    key: str
    value: Optional[float]
    unit: str = 'degC'
    status: str = 'ok'
    quality: str = 'verified'
    age_ms: int = 0
    ttl_ms: int = 3000
    origin: Optional[str] = None
    rank: int = 0
    kind: str = 'simulated'
    fault: bool = False
    alarm: bool = False
    aligned: bool = True

def normalize_temperature(value: Optional[float], unit: str) -> Optional[float]:
    if value is None or not isfinite(value):
        return None
    if unit == 'degC':
        return value
    if unit == 'mdegC':
        return value / 1000.0
    return None

def arbitrate(candidates: list[Candidate], key: str, threshold: float = 8.0,
              pin: Optional[str] = None, namespace: str = 'simulated') -> dict:
    """只驗證准入、同源去重與 conflict；不實作完整遲滯／平台 profile。"""
    if threshold <= 0 or not isfinite(threshold):
        raise ValueError('threshold 必須為正有限數')
    eligible = []
    rejected = {}
    for c in candidates:
        v = normalize_temperature(c.value, c.unit)
        reason = None
        if c.key != key: reason = 'different_scope'
        elif c.kind != namespace: reason = 'different_namespace'
        elif c.status != 'ok': reason = c.status
        elif c.fault: reason = 'driver_fault'
        elif c.quality not in ('verified','usable'): reason = c.quality
        elif c.age_ms > c.ttl_ms: reason = 'stale'
        elif v is None: reason = 'invalid_value_or_unit'
        if reason: rejected[c.source] = reason
        else: eligible.append((c,v))
    if pin is not None and not any(c.source == pin for c,_ in eligible):
        return dict(state='unavailable',source=None,value=None,reason='pinned_source_unavailable',rejected=rejected)
    if not eligible:
        return dict(state='unavailable',source=None,value=None,reason='no_eligible_source',rejected=rejected)
    eligible.sort(key=lambda p: (0 if p[0].quality=='verified' else 1,p[0].rank,p[0].age_ms,p[0].source))
    unique = []
    origins = set()
    for c,v in eligible:
        if c.origin is not None and c.origin in origins:
            rejected[c.source] = 'alias_not_independent'
            continue
        if c.origin is not None: origins.add(c.origin)
        unique.append((c,v))
    for i,(a,va) in enumerate(unique):
        for b,vb in unique[i+1:]:
            if a.origin is not None and b.origin is not None and a.origin != b.origin and a.aligned and b.aligned and abs(va-vb)>threshold:
                return dict(state='conflict',source=None,value=None,reason='source_disagreement',rejected=rejected)
    selected = next((p for p in eligible if p[0].source==pin),unique[0]) if pin else unique[0]
    return dict(state='selected',source=selected[0].source,value=selected[1],reason='valid_candidate',rejected=rejected)

@dataclass(frozen=True)
class Counter:
    value: int
    boot_ns: int
    boot_id: str
    source: str
    generation: int

def counter_rate(prev: Counter, current: Counter) -> Optional[float]:
    if (prev.boot_id,prev.source,prev.generation)!=(current.boot_id,current.source,current.generation): return None
    if current.boot_ns<=prev.boot_ns or current.value<prev.value: return None
    return (current.value-prev.value)/((current.boot_ns-prev.boot_ns)/1e9)

def weighted_window(points: list[tuple[int,float]], start: int, end: int, ttl: int) -> dict:
    """boot 秒級合成樣本；步階延伸有 TTL，不跨未覆蓋空窗。"""
    if end<=start or ttl<=0: raise ValueError('時窗／TTL 無效')
    points=sorted(points)
    weighted=0.0; covered=0
    for i,(t,v) in enumerate(points):
        nxt=points[i+1][0] if i+1<len(points) else end
        a=max(t,start);b=min(nxt,t+ttl,end)
        if b>a:
            weighted+=v*(b-a);covered+=b-a
    return {'mean':weighted/covered if covered else None,'covered':covered,'coverage':covered/(end-start)}

class ActionModel:
    """不做 OS 動作，只驗證兩階段狀態／拒絕條件與模擬送出次數。"""
    def __init__(self):
        self.token=None;self.sent_count=0;self.results={}
    def prepare(self, key: str, uid: int, target_uid: int, now: int, readonly=False, supported=True):
        if readonly or uid==0 or uid!=target_uid or not supported: return 'denied'
        self.token={'key':key,'expires':now+10,'used':False};return 'prepared'
    def confirm(self, key: str, now: int, request_id: str, audit_ok=True):
        if request_id in self.results:return self.results[request_id]
        t=self.token
        if t is None or t['used'] or now>=t['expires'] or key!=t['key'] or not audit_ok:return 'denied'
        t['used']=True;self.sent_count+=1;self.results[request_id]='signal_sent';return 'signal_sent'
