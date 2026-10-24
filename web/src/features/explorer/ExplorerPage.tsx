import { Link, Route, Routes } from 'react-router-dom';
import { SearchBar } from './SearchBar';
import { BlockList } from './BlockList';
import { MempoolPanel } from './MempoolPanel';
import { BlockDetail } from './BlockDetail';
import { TransactionDetail } from './TransactionDetail';
import { AddressDetail } from './AddressDetail';
import { ErrorState } from '@/components/ui/StateBlock';
import { BACK_LINK } from './back-link';

export function ExplorerPage() {
  return (
    <>
      <SearchBar />
      <Routes>
        <Route
          index
          element={
            <div className="grid gap-4">
              <MempoolPanel />
              <BlockList />
            </div>
          }
        />
        <Route path="block/:id" element={<BlockDetail />} />
        <Route path="tx/:txid" element={<TransactionDetail />} />
        <Route path="address/:address" element={<AddressDetail />} />
        {/* `/explorer/*` consumes the app-level catch-all, so without this an
            unknown path here renders the search bar above an empty page. */}
        <Route
          path="*"
          element={
            <ErrorState
              message="That page is not part of the explorer. Search above for a block height, block hash, transaction ID, or transparent address."
              action={
                <Link to="/explorer" className={BACK_LINK}>
                  Back to blocks
                </Link>
              }
            />
          }
        />
      </Routes>
    </>
  );
}
