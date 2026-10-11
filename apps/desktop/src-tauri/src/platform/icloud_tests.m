// Compiled by the macOS unit test only; not linked into the production App.
#import "icloud.m"
#include <assert.h>

int main(void) {
    @autoreleasepool {
        NSURL *url = [NSURL fileURLWithPath:@"/test/backup.hatoba-backup"];
        NSDictionary *ready = @{ @"downloaded": @YES };
        NSDictionary *pending = @{ @"downloaded": @NO };
        NSDictionary *failed = @{ @"error": @"injected status failure" };
        __block NSUInteger starts = 0, waits = 0, reads = 0;
        __block BOOL local = YES;
        BOOL (^available)(NSURL *) = ^BOOL(NSURL *file) { return local; };
        BOOL (^start)(NSURL *, NSError **) = ^BOOL(NSURL *file, NSError **error) {
            starts++; return YES;
        };
        void (^wait)(void) = ^{ waits++; };
        NSDictionary *result = downloadBackup(url, ^NSDictionary *(NSURL *file) {
            return ready;
        }, start, available, wait);
        assert([result[@"ok"] boolValue] && starts == 0);

        // An unreadable status must never be swallowed, even with a local file present.
        result = downloadBackup(url, ^NSDictionary *(NSURL *file) { return failed; },
            start, available, wait);
        assert(result[@"error"] && starts == 0);

        // A missing logical file with unknown status is given a materialization attempt.
        local = NO;
        result = downloadBackup(url, ^NSDictionary *(NSURL *file) {
            reads++; if (starts == 0) return failed;
            local = YES; return ready;
        }, start, available, wait);
        assert([result[@"ok"] boolValue] && starts == 1 && reads == 2);

        // A stale downloaded flag cannot stand in for a readable, materialized file.
        starts = waits = 0; local = NO;
        result = downloadBackup(url, ^NSDictionary *(NSURL *file) { return ready; },
            start, available, wait);
        assert(result[@"error"] && starts == 1 && waits == 150);

        // FALSE without NSError is still a failed download start.
        starts = waits = 0;
        result = downloadBackup(url, ^NSDictionary *(NSURL *file) { return pending; },
            ^BOOL(NSURL *file, NSError **error) { starts++; return NO; }, available, wait);
        assert(result[@"error"] && starts == 1 && waits == 0);

        // Even an anomalous TRUE accompanied by NSError must not become success.
        result = downloadBackup(url, ^NSDictionary *(NSURL *file) { return pending; },
            ^BOOL(NSURL *file, NSError **error) {
                *error = [NSError errorWithDomain:@"test" code:1 userInfo:nil]; return YES;
            }, available, wait);
        assert(result[@"error"]);

        // Failure to inspect after starting the download is reported immediately.
        starts = waits = 0;
        result = downloadBackup(url, ^NSDictionary *(NSURL *file) {
            return starts ? failed : pending;
        }, start, available, wait);
        assert(result[@"error"] && starts == 1 && waits == 0);

        // Pending materialization becomes successful automatically once the file is readable.
        starts = waits = 0;
        result = downloadBackup(url, ^NSDictionary *(NSURL *file) {
            if (waits >= 2) { local = YES; return ready; }
            return pending;
        }, start, available, wait);
        assert([result[@"ok"] boolValue] && starts == 1 && waits == 2);

        starts = waits = 0; local = NO;
        result = downloadBackup(url, ^NSDictionary *(NSURL *file) { return pending; },
            start, available, wait);
        assert(result[@"error"] && starts == 1 && waits == 150);
    }
    return 0;
}
