// Reports per browser, so an absent or unauthorised browser is distinguishable
// from one with no tabs. Treating those alike let a partial snapshot look
// authoritative and destroy rows for tabs it simply could not see.
function collect(name) {
    var report = { name: name, ok: false, reason: '', tabs: [] };
    var app;
    try { app = Application(name); } catch (e) { report.reason = 'not installed'; return report; }
    try { if (!app.running()) { report.reason = 'not running'; return report; } }
    catch (e) { report.reason = 'not scriptable: ' + e; return report; }
    try {
        app.windows().forEach(function (w, wi) {
            w.tabs().forEach(function (t, ti) {
                var url = '', title = '';
                try { url = String(t.url() || ''); } catch (e) { }
                try { title = String((name === 'Safari' ? t.name() : t.title()) || ''); } catch (e) { }
                if (url && url.indexOf('http') === 0) {
                    report.tabs.push({ browser: name, url: url, title: title,
                                       location: 'Window ' + (wi + 1) + ' / Tab ' + (ti + 1) });
                }
            });
        });
        report.ok = true;
    } catch (e) {
        report.reason = 'could not read windows: ' + e;
    }
    return report;
}
JSON.stringify({ browsers: [collect('Google Chrome'), collect('Safari')] });
