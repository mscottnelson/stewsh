// Browser tabs use the same Automation permission class as iTerm. Generic app
// windows would need Accessibility, which is a separate and broader grant.
function collect(name) {
    var out = [];
    try {
        var app = Application(name);
        if (!app.running()) return out;
        app.windows().forEach(function (w, wi) {
            var tabs = w.tabs();
            tabs.forEach(function (t, ti) {
                var url = '', title = '';
                try { url = String(t.url() || ''); } catch (e) { }
                try { title = String((name === 'Safari' ? t.name() : t.title()) || ''); } catch (e) { }
                if (url && url.indexOf('http') === 0) {
                    out.push({ browser: name, url: url, title: title,
                               location: 'Window ' + (wi + 1) + ' / Tab ' + (ti + 1) });
                }
            });
        });
    } catch (e) { }
    return out;
}
JSON.stringify(collect('Google Chrome').concat(collect('Safari')));
