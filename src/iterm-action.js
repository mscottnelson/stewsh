function run(argv) {
    var app = Application('iTerm2');
    if (!app.running()) throw Error('iTerm2 is not running');
    for (var w of app.windows()) for (var t of w.tabs()) for (var s of t.sessions()) {
        if (s.uniqueID() === argv[0]) {
            if (argv[1] === 'preview') return JSON.stringify({text:s.contents()});
            t.select(); s.select(); w.index=1; app.activate();
            return JSON.stringify({focused:true});
        }
    }
    throw Error('The pane is no longer open; run sync');
}
