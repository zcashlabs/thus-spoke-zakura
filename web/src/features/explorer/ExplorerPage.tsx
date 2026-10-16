import { Link, Route, Routes, useLocation } from 'react-router-dom';
import { SearchBar } from './SearchBar';
import { BlockList } from './BlockList';
import { BlockDetail } from './BlockDetail';
import { TransactionDetail } from './TransactionDetail';
import { AddressDetail } from './AddressDetail';
import { ErrorState } from '@/components/ui/StateBlock';
import { BACK_LINK } from './back-link';

export function ExplorerPage() {
  const { key } = useLocation();

  return (
    <>
      {/* A fresh search bar per navigation, even to the same page, drops any pending search. */}
      <SearchBar key={key} />
      <Routes>
        <Route index element={<BlockList />} />
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
