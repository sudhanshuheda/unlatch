import FileProvider
import Foundation

/// Collects every identifier of `enumeratorForMaterializedItems()` (review D5: the working set is
/// reported against the *materialized* set M, which the engine learns only from this walk).
public final class MaterializedSetWalker: NSObject, NSFileProviderEnumerationObserver {
    private let enumerator: NSFileProviderEnumerator
    private var ids: [ItemId] = []
    private var completion: ((Result<[ItemId], Error>) -> Void)?
    private var pages = 0

    private init(enumerator: NSFileProviderEnumerator) {
        self.enumerator = enumerator
    }

    /// Walk all pages. `completion` runs exactly once, on an arbitrary queue.
    public static func collect(manager: NSFileProviderManager, completion: @escaping (Result<[ItemId], Error>) -> Void) {
        let walker = MaterializedSetWalker(enumerator: manager.enumeratorForMaterializedItems())
        walker.completion = { result in
            walker.enumerator.invalidate()
            completion(result)
        }
        walker.next(NSFileProviderPage(NSFileProviderPage.initialPageSortedByName as Data))
    }

    private func next(_ page: NSFileProviderPage) {
        pages += 1
        enumerator.enumerateItems(for: self, startingAt: page)
    }

    public func didEnumerate(_ updatedItems: [NSFileProviderItemProtocol]) {
        ids.append(contentsOf: updatedItems.compactMap { ItemIdentifiers.itemId(for: $0.itemIdentifier) })
    }

    public func finishEnumerating(upTo nextPage: NSFileProviderPage?) {
        // A runaway enumerator must not loop forever; 100k pages is far beyond any real set.
        if let nextPage, pages < 100_000 {
            next(nextPage)
        } else {
            finish(.success(ids))
        }
    }

    public func finishEnumeratingWithError(_ error: Error) {
        finish(.failure(error))
    }

    private func finish(_ result: Result<[ItemId], Error>) {
        let c = completion
        completion = nil
        c?(result)
    }
}
