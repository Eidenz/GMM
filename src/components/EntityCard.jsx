// src/components/EntityCard.jsx
import React from 'react';
import { Link } from 'react-router-dom';

// Helper function to parse details JSON
const parseDetails = (detailsJson) => {
    try {
        if (!detailsJson) return {};
        return JSON.parse(detailsJson);
    } catch (e) {
        console.error("Failed to parse entity details JSON:", e);
        return {}; // Return empty object on error
    }
};

const getRarityColor = (value) => {
    if (!value) return '#888'; // Default color for unknown rarity
    const val = value.toLowerCase();
    if (val === '5 star' || val === 's') return '#ffcc00'; // Gold
    if (val === '4 star' || val === 'a') return '#a259ec'; // Purple
    return '#888'; // Gray for fallback
}

// Font Awesome icons map for Genshin elements
const elementIconsFA = {
    Electro: "fas fa-bolt", Pyro: "fas fa-fire", Cryo: "fas fa-snowflake",
    Hydro: "fas fa-tint", Anemo: "fas fa-wind", Geo: "fas fa-mountain",
    Dendro: "fas fa-leaf",
};

// Font Awesome icons map for ZZZ attributes
const attributeIconsFA = {
    Physical: "fas fa-fist-raised",
    Fire: "fas fa-fire-alt",
    Ice: "fas fa-icicles",
    Electric: "fas fa-bolt",
    AuricInk: "fas fa-paint-brush",
    // Add more ZZZ attributes as needed
};

// Font Awesome icons map for ZZZ specialties
const specialtyIconsFA = {
    Assault: "fas fa-crosshairs",
    Support: "fas fa-hands-helping",
    Defense: "fas fa-shield-alt",
    Healer: "fas fa-first-aid",
    Rupture: "fas fa-bomb",
    // Add more ZZZ specialties as needed
};

// Font Awesome icons map for Wuwa resonator attributes
const resonatorIconsFA = {
    Aero: "fas fa-wind",
    Electro: "fas fa-bolt",
    Fusion: "fas fa-fire-flame-curved",
    Glacio: "fas fa-snowflake",
    Havoc: "fas fa-explosion",
    Spectro: "fas fa-sun",
    // Add more Wuwa resonator attributes as needed
};

// Icons for common displays
const RarityIcon = ({ value }) => <i className="fas fa-star fa-fw" style={{ color: getRarityColor(value) }}></i>;
const TypeIcon = () => <i className="fas fa-tag fa-fw" style={{ color: '#7acbf9' }}></i>;
const DEFAULT_PLACEHOLDER_IMAGE = '/images/unknown.jpg';

function EntityCard({ entity }) {
    // Destructure props including counts
    const { slug, name, details: detailsJson, base_image, total_mods, enabled_mods } = entity;

    const details = parseDetails(detailsJson);
    
    // Genshin-specific properties
    const element = details?.element;
    const elementIconClass = element ? (elementIconsFA[element] || 'fas fa-question-circle') : null;
    
    // ZZZ-specific properties
    const attribute = details?.attribute;
    const attributeIconClass = attribute ? (attributeIconsFA[attribute] || 'fas fa-atom') : null;
    const specialty = details?.specialty;
    const specialtyIconClass = specialty ? (specialtyIconsFA[specialty] || 'fas fa-user-tag') : null;
    const types = details?.types || [];

    // Wuwa-specific properties
    const wuwaAttribute = details?.resonator_attribute;
    const wuwaWeapon = details?.resonator_weapon;
    const wuwaAttributeIconClass = wuwaAttribute ? (resonatorIconsFA[wuwaAttribute] || 'fas fa-question-circle') : null;

    const imageUrl = base_image ? `/images/entities/${slug}_base.jpg` : DEFAULT_PLACEHOLDER_IMAGE;

    const handleImageError = (e) => {
        // If the specific image fails, fall back to the generic placeholder
        if (e.target.src !== DEFAULT_PLACEHOLDER_IMAGE) {
            console.warn(`Failed to load base image: ${imageUrl}, falling back to placeholder.`);
            e.target.src = DEFAULT_PLACEHOLDER_IMAGE;
        }
    };

    // Determine if this is a ZZZ character (has attribute or specialty)
    const isZZZ = attribute || specialty || types.length > 0 || details?.rank;
    
    // Determine if this is a Wuwa character
    const isWuwa = wuwaAttribute || wuwaWeapon;

    return (
        <Link to={`/entity/${slug}`} className={`character-card ${isZZZ ? 'zzz-card' : isWuwa ? 'wuwa-card' : 'genshin-card'}`} title={`View mods for ${name}`}>

            {/* Container for Badges (CSS will handle layout) */}
            <div className="card-badges-container">
                 {/* Total Mod Count Badge */}
                {total_mods > 0 && (
                    <div className="card-badge total-badge" title={`${total_mods} total mods`}>
                        {total_mods} <i className="fas fa-box fa-fw" style={{ marginLeft: '3px', opacity: 0.8 }}></i>
                    </div>
                )}
                  {/* Enabled Mod Count Badge */}
                {enabled_mods > 0 && (
                    <div className="card-badge enabled-badge" title={`${enabled_mods} mods enabled`}>
                        {enabled_mods} <i className="fas fa-check-circle fa-fw" style={{ marginLeft: '3px' }}></i>
                    </div>
                )}
            </div>

            {/* Card Image */}
            <div
                className="card-image"
                style={{ backgroundImage: `url('${imageUrl}')` }}
                // onError could potentially be added here if using <img> instead of background
            ></div>

            {/* Card Content */}
            <div className="card-content">
                <div className="card-name">{name}</div>
                
                {/* Genshin-specific properties */}
                {element && elementIconClass && (
                    <div className="card-element" title={element}>
                        <i className={`${elementIconClass} fa-fw`} style={{ color: `var(--${element?.toLowerCase()})` || 'var(--light)' }}></i>
                        {element}
                    </div>
                )}
                
                {/* ZZZ-specific properties */}
                {attribute && attributeIconClass && (
                    <div className="card-attribute" title={`Attribute: ${attribute}`}>
                        <i className={`${attributeIconClass} fa-fw`} style={{ color: `var(--zzz-${attribute?.toLowerCase()})` || 'var(--light)' }}></i>
                        {attribute}
                    </div>
                )}

                {/* Wuwa-specific properties */}
                {wuwaAttribute && wuwaAttributeIconClass && (
                    <div className="card-element" title={wuwaAttribute}>
                        <i className={`${wuwaAttributeIconClass} fa-fw`} style={{ color: `var(--wuwa-${wuwaAttribute?.toLowerCase()})` || 'var(--light)' }}></i>
                        {wuwaAttribute}
                    </div>
                )}
                
                {/* Shared properties with different styling */}
                {details?.rarity && (
                    <div className="card-element" style={{ marginTop: '5px', fontSize: '13px' }}>
                        <RarityIcon value={details.rarity} /> {details.rarity}
                    </div>
                )}
                
                {details?.rank && (
                    <div className="card-rank" style={{ marginTop: '5px', fontSize: '13px' }}>
                        <i className="fas fa-medal fa-fw" style={{ color: getRarityColor(details.rank) }}></i> Rank {details.rank}
                    </div>
                )}
            </div>
        </Link>
    );
}

export default EntityCard;