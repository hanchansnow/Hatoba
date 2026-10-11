#import <Foundation/Foundation.h>
#include <stdlib.h>
#include <string.h>

// Only paths and transfer metadata cross this bridge. Vault contents remain encrypted.
static BOOL flag(id value) {
    return [value isKindOfClass:NSNumber.class] && [value boolValue];
}
static NSDictionary *cloudState(NSDictionary *values) {
    BOOL cloud = flag(values[NSURLIsUbiquitousItemKey]);
    NSString *download = values[NSURLUbiquitousItemDownloadingStatusKey];
    return @{ @"ubiquitous": @(cloud),
        @"uploading": @(flag(values[NSURLUbiquitousItemIsUploadingKey])),
        @"uploaded": @(flag(values[NSURLUbiquitousItemIsUploadedKey])),
        @"upload_error": @([values[NSURLUbiquitousItemUploadingErrorKey] isKindOfClass:NSError.class]),
        @"downloaded": @(!cloud || [download isEqual:NSURLUbiquitousItemDownloadingStatusCurrent]
            || [download isEqual:NSURLUbiquitousItemDownloadingStatusDownloaded]) };
}
static NSDictionary *inspect(NSURL *url) {
    // Background callers must discard cached values before checking transfer progress again.
    [url removeAllCachedResourceValues];
    NSError *error = nil;
    NSDictionary *values = [url resourceValuesForKeys:@[NSURLIsUbiquitousItemKey,
        NSURLUbiquitousItemIsUploadingKey, NSURLUbiquitousItemIsUploadedKey,
        NSURLUbiquitousItemDownloadingStatusKey, NSURLUbiquitousItemUploadingErrorKey] error:&error];
    if (!values) return @{ @"error": @"Unable to read iCloud file status." };
    return cloudState(values);
}

char *hatoba_icloud(const char *input) {
    @autoreleasepool {
        NSData *data = [NSData dataWithBytes:input length:strlen(input)];
        NSDictionary *request = [NSJSONSerialization JSONObjectWithData:data options:0 error:nil];
        NSDictionary *result = nil;
        NSURL *url = [NSURL fileURLWithPath:request[@"path"] ?: @""];
        NSString *operation = request[@"operation"];
        NSFileManager *fm = NSFileManager.defaultManager;
        if ([operation isEqual:@"inspect"]) {
            result = inspect(url);
        } else if ([operation isEqual:@"download"]) {
            NSError *error = nil;
            NSDictionary *state = inspect(url);
            if ([state[@"ubiquitous"] boolValue] && ![state[@"downloaded"] boolValue]) {
                [fm startDownloadingUbiquitousItemAtURL:url error:&error];
                // Poll outside file coordination, so the iCloud daemon can finish its write.
                NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:30];
                while (!error && ![inspect(url)[@"downloaded"] boolValue]
                        && deadline.timeIntervalSinceNow > 0) [NSThread sleepForTimeInterval:0.2];
                if (!error && ![inspect(url)[@"downloaded"] boolValue]) {
                    result = @{ @"error": @"The iCloud file is still downloading. Try again shortly." };
                }
            }
            if (!result) result = error ? @{ @"error": @"Unable to download the iCloud backup." } : @{ @"ok": @YES };
        } else if ([operation isEqual:@"move"]) {
            NSURL *other = [NSURL fileURLWithPath:request[@"other"] ?: @""];
            NSFileCoordinator *coordinator = [[NSFileCoordinator alloc] initWithFilePresenter:nil];
            NSError *coordination = nil;
            __block NSError *io = nil;
            [coordinator coordinateWritingItemAtURL:url options:NSFileCoordinatorWritingForMoving
                writingItemAtURL:other options:0 error:&coordination
                byAccessor:^(NSURL *source, NSURL *target) {
                    [fm moveItemAtURL:source toURL:target error:&io];
                    if (!io) [coordinator itemAtURL:source didMoveToURL:target];
                }];
            result = (coordination || io) ? @{ @"error": @"Unable to migrate an older backup." } : @{ @"ok": @YES };
        } else if ([operation isEqual:@"remove"]) {
            NSFileCoordinator *coordinator = [[NSFileCoordinator alloc] initWithFilePresenter:nil];
            NSError *coordination = nil;
            __block NSError *io = nil;
            [coordinator coordinateWritingItemAtURL:url options:NSFileCoordinatorWritingForDeleting
                error:&coordination byAccessor:^(NSURL *target) {
                    [fm removeItemAtURL:target error:&io];
                }];
            result = (coordination || io) ? @{ @"error": @"Unable to remove an older backup." } : @{ @"ok": @YES };
        } else if ([operation isEqual:@"publish"] || [operation isEqual:@"read"]) {
            NSURL *other = [NSURL fileURLWithPath:request[@"other"] ?: @""];
            NSFileCoordinator *coordinator = [[NSFileCoordinator alloc] initWithFilePresenter:nil];
            NSError *coordination = nil;
            __block NSError *io = nil;
            if ([operation isEqual:@"publish"]) {
                [coordinator coordinateWritingItemAtURL:url options:0 error:&coordination byAccessor:^(NSURL *target) {
                    NSURL *temporary = [target.URLByDeletingLastPathComponent URLByAppendingPathComponent:
                        [NSString stringWithFormat:@".%@.tmp", NSUUID.UUID.UUIDString]];
                    if ([fm copyItemAtURL:other toURL:temporary error:&io]) {
                        [fm moveItemAtURL:temporary toURL:target error:&io];
                    }
                    [fm removeItemAtURL:temporary error:nil];
                }];
            } else {
                [coordinator coordinateReadingItemAtURL:url options:0 error:&coordination byAccessor:^(NSURL *source) {
                    [fm copyItemAtURL:source toURL:other error:&io];
                }];
            }
            result = (coordination || io) ? @{ @"error": @"Unable to access the backup file. Check folder permissions and available space." } : @{ @"ok": @YES };
        }
        if (!result) result = @{ @"error": @"Unsupported iCloud operation." };
        NSData *output = [NSJSONSerialization dataWithJSONObject:result options:0 error:nil];
        char *buffer = malloc(output.length + 1);
        if (!buffer) return NULL;
        memcpy(buffer, output.bytes, output.length);
        buffer[output.length] = 0;
        return buffer;
    }
}
void hatoba_icloud_free(char *buffer) { free(buffer); }
