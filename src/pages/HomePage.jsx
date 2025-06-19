// src/pages/HomePage.jsx
import React, { useState, useEffect, useMemo, useCallback } from 'react';
import { useParams } from 'react-router-dom';
import { invoke } from '@tauri-apps/api/tauri';
import EntityCard from '../components/EntityCard';
import { getLocalStorageItem, setLocalStorageItem } from '../utils/localStorage';
import EntityCardSkeleton from '../components/EntityCardSkeleton';

// Element data for Genshin
const elements = [
    { key: 'all', name: 'All', icon: 'fas fa-circle-nodes', color: 'var(--light)' },
    { key: 'Pyro', name: 'Pyro', icon: 'fas fa-fire', color: 'var(--pyro)' },
    { key: 'Hydro', name: 'Hydro', icon: 'fas fa-tint', color: 'var(--hydro)' },
    { key: 'Anemo', name: 'Anemo', icon: 'fas fa-wind', color: 'var(--anemo)' },
    { key: 'Electro', name: 'Electro', icon: 'fas fa-bolt', color: 'var(--electro)' },
    { key: 'Dendro', name: 'Dendro', icon: 'fas fa-leaf', color: 'var(--dendro)' },
    { key: 'Cryo', name: 'Cryo', icon: 'fas fa-snowflake', color: 'var(--cryo)' },
    { key: 'Geo', name: 'Geo', icon: 'fas fa-mountain', color: 'var(--geo)' },
];

// Attribute data for ZZZ
const attributes = [
    { key: 'all', name: 'All', icon: 'fas fa-circle-nodes', color: 'var(--light)' },
    { key: 'Electric', name: 'Electric', icon: 'fas fa-bolt', color: 'var(--zzz-electric)' },
    { key: 'Fire', name: 'Fire', icon: 'fas fa-fire', color: 'var(--zzz-fire)' },
    { key: 'Ice', name: 'Ice', icon: 'fas fa-snowflake', color: 'var(--zzz-ice)' },
    { key: 'Frost', name: 'Frost', icon: 'fas fa-snowflake', color: 'var(--zzz-ice)' },
    { key: 'Ether', name: 'Ether', icon: 'fas fa-star', color: 'var(--zzz-ether)' },
    { key: 'Physical', name: 'Physical', icon: 'fas fa-fist-raised', color: 'var(--zzz-physical)' },
    { key: 'AuricInk', name: 'Auric Ink', icon: 'fas fa-paint-brush', color: 'var(--zzz-auric-ink)' },
];

// Attribute data for Wuwa
const resonatorAttributes = [
    { key: 'all',    name: 'All',    icon: 'fas fa-circle-nodes', color: 'var(--light)' },
    { key: 'Aero',   name: 'Aero',   icon: 'fas fa-wind',         color: 'var(--wuwa-aero)' },
    { key: 'Electro',name: 'Electro',icon: 'fas fa-bolt',         color: 'var(--wuwa-electro)' },
    { key: 'Fusion', name: 'Fusion', icon: 'fas fa-fire-flame-curved', color: 'var(--wuwa-fusion)' },
    { key: 'Glacio', name: 'Glacio', icon: 'fas fa-snowflake',    color: 'var(--wuwa-glacio)' },
    { key: 'Havoc',  name: 'Havoc',  icon: 'fas fa-explosion',    color: 'var(--wuwa-havoc)' },
    { key: 'Spectro',name: 'Spectro',icon: 'fas fa-sun',          color: 'var(--wuwa-spectro)' },
];

// Helper to safely parse JSON
const safeParseJson = (jsonString, defaultValue = null) => {
    if (!jsonString) return defaultValue;
    try {
        return JSON.parse(jsonString);
    } catch (e) {
        console.error("JSON parse error:", e);
        return defaultValue;
    }
};

// Sorting Options including new ones
const sortOptions = [
    { value: 'name-asc', label: 'Name (A-Z)' },
    { value: 'name-desc', label: 'Name (Z-A)' },
    { value: 'total-desc', label: 'Total Mods (High-Low)' },
    { value: 'total-asc', label: 'Total Mods (Low-High)' },
    { value: 'enabled-desc', label: 'Enabled Mods (High-Low)' },
    { value: 'enabled-asc', label: 'Enabled Mods (Low-High)' },
];
const DEFAULT_SORT_OPTION = 'name-asc';
const OTHER_ENTITY_SUFFIX = '-other'; // Make sure this matches backend

function HomePage() {
    const { categorySlug } = useParams();
    const [categoryInfo, setCategoryInfo] = useState({ name: categorySlug, id: null });
    // State holds the new structure returned by the backend
    const [entitiesWithCounts, setEntitiesWithCounts] = useState([]);
    const [loadingEntities, setLoadingEntities] = useState(true);
    const [error, setError] = useState(null);
    const [searchTerm, setSearchTerm] = useState('');
    const [selectedElement, setSelectedElement] = useState('all');
    const [selectedAttribute, setSelectedAttribute] = useState('all');
    const [selectedResonatorAttr, setSelectedResonatorAttr] = useState('all');
    const [sortOption, setSortOption] = useState(DEFAULT_SORT_OPTION);
    const [activeGame, setActiveGame] = useState('genshin');
    const sortStorageKey = `categorySort_${categorySlug}`;

    // Fetch Active Game Info
    useEffect(() => {
        const fetchActiveGame = async () => {
            try {
            const game = await invoke('get_active_game');
            setActiveGame(game || 'genshin');
            } catch (err) {
            console.error("Failed to get active game:", err);
            setActiveGame('genshin');
            }
        };
        
        fetchActiveGame();
    }, []);

    // Fetch Category Info and Entities with Counts
    useEffect(() => {
        setLoadingEntities(true);
        setError(null);
        setEntitiesWithCounts([]); // Clear previous entities
        setSelectedElement('all');
        setSelectedAttribute('all');
        setSearchTerm('');
        const savedSort = getLocalStorageItem(sortStorageKey, DEFAULT_SORT_OPTION);
        setSortOption(savedSort);
        // Simple category name update, could fetch real name later if needed
        setCategoryInfo({ name: categorySlug ? categorySlug.charAt(0).toUpperCase() + categorySlug.slice(1) : 'Unknown', id: null });

        // Call the new backend command
        invoke('get_entities_by_category_with_counts', { categorySlug })
            .then(fetchedData => {
                // Add console log to verify data structure
                setEntitiesWithCounts(fetchedData || []); // Ensure it's an array
            })
            .catch(err => {
                console.error(`Failed to fetch entities with counts for ${categorySlug}:`, err);
                setError(`Could not load ${categorySlug}. Details: ${typeof err === 'string' ? err : err.message || 'Unknown error'}`);
            })
            .finally(() => setLoadingEntities(false));

    }, [categorySlug, sortStorageKey, activeGame]); // Dependencies for fetching data

    // Handle Sort Change
    const handleSortChange = (event) => {
        const newSortOption = event.target.value;
        setSortOption(newSortOption);
        setLocalStorageItem(sortStorageKey, newSortOption);
    };

    // Memoized filtered AND sorted list
    const filteredAndSortedEntities = useMemo(() => {
        // Start with the fetched data
        let tempEntities = [...entitiesWithCounts]; // Create a copy to sort

        // Filtering Logic
        tempEntities = tempEntities.filter(entity => {
            // Element Filter (only for Genshin characters category)
            if (categorySlug === 'characters' && activeGame === 'genshin' && selectedElement !== 'all') {
                const details = safeParseJson(entity.details, {});
                if (details?.element !== selectedElement) return false;
            }
            
            // Attribute Filter (only for ZZZ characters category)
            if (categorySlug === 'characters' && activeGame === 'zzz' && selectedAttribute !== 'all') {
                const details = safeParseJson(entity.details, {});
                if (details?.attribute !== selectedAttribute) return false;
            }
            
            // Attribute Filter (only for Wuwa characters category)
            if (categorySlug === 'characters' && activeGame === 'wuwa' && selectedResonatorAttr !== 'all') {
                const details = safeParseJson(entity.details, {});
                if (details?.resonator_attribute !== selectedResonatorAttr) return false;
            }
            
            // Search Term Filter
            if (searchTerm) {
                const lowerSearch = searchTerm.toLowerCase();
                if (!entity.name.toLowerCase().includes(lowerSearch)) {
                     return false; // Exclude if name doesn't match
                }
            }
            return true; // Include if passes all filters
        });

        // Sorting Logic with "Other" priority
        tempEntities.sort((a, b) => {
            const isAOther = a.slug.endsWith(OTHER_ENTITY_SUFFIX);
            const isBOther = b.slug.endsWith(OTHER_ENTITY_SUFFIX);

            // Prioritize "Other" group first
            if (isAOther && !isBOther) return -1;
            if (!isAOther && isBOther) return 1;

            // Apply selected sort (within 'Other' group or for non-'Other' items)
            // Use nullish coalescing (?? 0) to safely handle potential undefined/null counts
            switch (sortOption) {
                case 'name-asc': return a.name.localeCompare(b.name);
                case 'name-desc': return b.name.localeCompare(a.name);
                case 'total-desc': return (b.total_mods ?? 0) - (a.total_mods ?? 0);
                case 'total-asc': return (a.total_mods ?? 0) - (b.total_mods ?? 0);
                case 'enabled-desc': return (b.enabled_mods ?? 0) - (a.enabled_mods ?? 0);
                case 'enabled-asc': return (a.enabled_mods ?? 0) - (b.enabled_mods ?? 0);
                default: return a.name.localeCompare(b.name); // Fallback to name ascending
            }
        });

        return tempEntities;
    }, [entitiesWithCounts, searchTerm, selectedElement, selectedAttribute, selectedResonatorAttr, categorySlug, sortOption, activeGame]); // Dependencies for memoization


    const pageTitle = categoryInfo.name; // Use state for title
    const showElementFilters = categorySlug === 'characters' && activeGame === 'genshin';
    const showAttributeFilters = categorySlug === 'characters' && activeGame === 'zzz';
    const showWuwaAttributeFilters = categorySlug === 'characters' && activeGame === 'wuwa';

    return (
        <div className="home-page fadeIn">
            <div className="page-header">
                <h1 className="page-title">{pageTitle}</h1>

                 {/* Sort Dropdown */}
                <div className="sort-dropdown-container" style={{ 
                    marginLeft: (showElementFilters || showAttributeFilters || showWuwaAttributeFilters) ? '20px' : 'auto', 
                    marginRight: '20px' 
                }}>
                    <label htmlFor="sort-select" style={styles.sortLabel}>Sort by:</label>
                    <select id="sort-select" value={sortOption} onChange={handleSortChange} style={styles.sortSelect} aria-label="Sort entities">
                        {sortOptions.map(option => ( <option key={option.value} value={option.value}>{option.label}</option> ))}
                    </select>
                </div>

                 {/* Element Filters (Conditional for Genshin) */}
                {showElementFilters && (
                    <div className="element-filters">
                        {elements.map(element => (
                            <button
                                key={element.key}
                                className={`element-filter-button ${selectedElement === element.key ? 'active' : ''}`}
                                onClick={() => setSelectedElement(element.key)}
                                title={element.name}
                                style={{ '--element-color': element.color }}
                            >
                                <i className={`${element.icon} fa-fw`}></i>
                                <span className="filter-button-name">{element.name}</span>
                            </button>
                        ))}
                    </div>
                )}

                 {/* Attribute Filters (Conditional for ZZZ) */}
                {showAttributeFilters && (
                    <div className="attribute-filters">
                        {attributes.map(attribute => (
                            <button
                                key={attribute.key}
                                className={`attribute-filter-button ${selectedAttribute === attribute.key ? 'active' : ''}`}
                                onClick={() => setSelectedAttribute(attribute.key)}
                                title={attribute.name}
                                style={{ '--attribute-color': attribute.color }}
                            >
                                <i className={`${attribute.icon} fa-fw`}></i>
                                <span className="filter-button-name">{attribute.name}</span>
                            </button>
                        ))}
                    </div>
                )}

                 {/* Attribute Filters (Conditional for Wuwa) */}
                {showWuwaAttributeFilters && (
                    <div className="attribute-filters">
                        {resonatorAttributes.map(attribute => (
                            <button
                                key={attribute.key}
                                className={`attribute-filter-button ${selectedResonatorAttr === attribute.key ? 'active' : ''}`}
                                onClick={() => setSelectedResonatorAttr(attribute.key)}
                                title={attribute.name}
                                style={{ '--attribute-color': attribute.color }}
                            >
                                <i className={`${attribute.icon} fa-fw`}></i>
                                <span className="filter-button-name">{attribute.name}</span>
                            </button>
                        ))}
                    </div>
                )}

                 {/* Search Bar Container */}
                <div className="search-bar-container">
                    <div className="search-bar">
                        <i className="fas fa-search"></i>
                        <input
                            type="text"
                            placeholder={`Search ${pageTitle ? pageTitle.toLowerCase() : 'items'}...`}
                            value={searchTerm}
                            onChange={(e) => setSearchTerm(e.target.value)}
                            aria-label={`Search ${pageTitle}`}
                            data-global-search="true"
                        />
                    </div>
                </div>
            </div>

            {/* Content Area */}
            {loadingEntities ? (
                <div className="cards-grid">
                    {/* Render Skeleton loaders */}
                    {Array.from({ length: 12 }).map((_, i) => <EntityCardSkeleton key={i} />)}
                </div>
            ) : error ? (
                <div className="placeholder-text" style={{ color: 'var(--danger)' }}>Error: {error}</div>
            ) : (
                <div className="cards-grid">
                    {filteredAndSortedEntities.length > 0 ? (
                        filteredAndSortedEntities.map(entityData => (
                            <EntityCard key={entityData.slug} entity={entityData} />
                        ))
                    ) : entitiesWithCounts.length > 0 ? (
                        <p className="placeholder-text" style={{ gridColumn: '1 / -1' }}>
                            No {pageTitle ? pageTitle.toLowerCase() : 'items'} found matching your criteria.
                        </p>
                    ) : (
                        <p className="placeholder-text" style={{ gridColumn: '1 / -1' }}>
                            No {pageTitle ? pageTitle.toLowerCase() : 'items'} have been added yet.
                        </p>
                    )}
                </div>
            )}
        </div>
    );
}

// Styles for sort dropdown
const styles = {
    sortLabel: {
        fontSize: '13px',
        color: 'rgba(255, 255, 255, 0.7)',
        marginRight: '8px',
        whiteSpace: 'nowrap', // Prevent label wrapping
    },
    sortSelect: {
        padding: '6px 10px',
        backgroundColor: 'rgba(0,0,0,0.3)',
        border: '1px solid rgba(255, 255, 255, 0.1)',
        borderRadius: '6px',
        color: 'var(--light)',
        fontSize: '13px',
        cursor: 'pointer',
        minWidth: '180px', // Adjust width as needed
        height: '34px', // Match height with filter buttons roughly
    },
};


export default HomePage;